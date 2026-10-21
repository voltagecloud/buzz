//! Channels, threads and reminders as the user: starting a DM, making,
//! joining and leaving channels (buzz-app's `directMessages`,
//! `channelCreation` and `channelLifecycle`), following threads
//! (`unread.follow`), and reminders (`reminders`). Changing channels needs
//! the `manage-channels` permission.

use super::capabilities::{Failure, Watch, channel, community, on_tokio, readable};
use super::protocol::INVALID_PARAMS;
use buzz_relay_client::lifecycle::{Action, NewChannel};
use buzz_relay_client::{EventId, Reminder, ReminderStatus, ReminderTarget};
use gpui_kit::*;
use serde_json::{Value, json};

fn invalid(message: impl Into<String>) -> Failure {
    (INVALID_PARAMS, message.into())
}

fn text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn event_id(params: &Value, key: &str) -> Result<EventId, Failure> {
    text(params, key)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or_else(|| invalid(format!("{key} is not an event id")))
}

/// `dm/open`: the direct message with `pubkeys` (1 to 8 others), made if
/// it is new. Answers with its channel id.
pub fn open_dm(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let keys = params
        .get("pubkeys")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("pubkeys must be a list"))?;
    let session = community(cx)?.read(cx).session.clone();
    // `validDirectMessageParticipants`: one to eight other people, each
    // once, as lowercase hex.
    let viewer = session.viewer();
    let keys: Vec<String> = keys
        .iter()
        .map(|key| {
            key.as_str()
                .filter(|key| {
                    key.len() == 64
                        && key
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                        && *key != viewer
                })
                .map(str::to_owned)
        })
        .collect::<Option<_>>()
        .filter(|keys: &Vec<String>| {
            (1..=8).contains(&keys.len())
                && keys.iter().collect::<std::collections::HashSet<_>>().len() == keys.len()
        })
        .ok_or_else(|| invalid("Choose between one and eight other people."))?;
    Ok(on_tokio(cx, async move {
        let id = session.open_dm(keys).await.map_err(|error| (3, error))?;
        Ok(json!({ "channel": id }))
    }))
}

/// `channels/create`: a new stream channel. Answers with its id.
pub fn create(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let name = text(params, "name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid("name is required"))?
        .to_owned();
    // At most 8, as the relay takes (`MAX_CHANNEL_LABELS`); more is refused.
    let labels: Vec<String> = match params.get("labels").filter(|labels| !labels.is_null()) {
        None => Vec::new(),
        Some(labels) => labels
            .as_array()
            .filter(|labels| labels.len() <= 8)
            .ok_or_else(|| invalid("labels must be a list of at most 8"))?
            .iter()
            .map(|label| {
                label
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("labels must be text"))
            })
            .collect::<Result<_, _>>()?,
    };
    let channel = NewChannel {
        name,
        description: text(params, "description").unwrap_or_default().to_owned(),
        private: params
            .get("private")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ttl: super::capabilities::ttl_seconds(
            params,
            "ttlSeconds",
            "Choose a valid temporary channel duration.",
        )?,
        labels,
    };
    // What a template adds (`setup`): a Canvas, agents, a sidebar group.
    let setup = match params.get("setup").filter(|setup| !setup.is_null()) {
        None => None,
        Some(setup) => {
            let field = |key: &str| text(setup, key).unwrap_or_default().to_owned();
            Some(buzz_relay_client::kit::setup::ChannelSetup {
                canvas: field("canvas"),
                agents: setup
                    .get("agents")
                    .and_then(Value::as_array)
                    .map(|agents| {
                        agents
                            .iter()
                            .map(|key| {
                                super::capabilities::pubkey(&json!({ "key": key }), "key")
                                    .map(str::to_owned)
                                    .ok_or_else(|| invalid("setup.agents must be agent keys"))
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                group_id: field("group"),
                template_id: field("template"),
            })
        }
    };
    let community = community(cx)?;
    let session = community.read(cx).session.clone();
    // A lineup's agents must be ones a template may add, by a fresh
    // archive read (`templateAgentChoices`).
    let lineup = setup
        .as_ref()
        .map(|setup| setup.agents.clone())
        .filter(|agents| !agents.is_empty());
    let checked = match lineup {
        None => Task::ready(Ok(())),
        Some(agents) => lineup_checked(community, agents, cx),
    };
    Ok(cx.spawn(async move |cx| {
        checked.await?;
        cx.update(|cx| create_now(session, channel, setup, cx))
            .await
    }))
}

/// Refuses a lineup with an agent a template may not add: not one of
/// your agents here, or archived, or while the archive is unknown.
fn lineup_checked(
    community: Entity<crate::community::Community>,
    agents: Vec<String>,
    cx: &mut App,
) -> Task<Result<(), Failure>> {
    use crate::ui::archives::{self, Status};
    archives::ensure(&community, true, cx);
    cx.spawn(async move |cx| {
        for _ in 0..150 {
            if cx.update(|cx| archives::status(&community, cx)) != Some(Status::Loading) {
                break;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
        cx.update(|cx| {
            if archives::status(&community, cx) != Some(Status::Ready) {
                return Err((
                    3,
                    "Agent archive state is unavailable; refresh before creating this lineup"
                        .to_owned(),
                ));
            }
            // Your agents this device runs here, none archived: Buzz binds
            // its local agents, so a template takes those only
            // (`templateAgentChoices` with a local source).
            let rules = crate::ui::me::recipients::rules(&community, cx);
            let origin = community.read(cx).origin.clone();
            let managed: std::collections::HashSet<String> =
                crate::ui::identity::AgentLibrary::from_hosts(cx)
                    .agents
                    .into_iter()
                    .filter(|(_, _, relay)| {
                        buzz_relay_client::RelayOrigin::parse(relay)
                            .is_ok_and(|other| other == origin)
                    })
                    .map(|(key, _, _)| key)
                    .collect();
            let available = |key: &String| rules.selectable.contains(key) && managed.contains(key);
            let unavailable: Vec<String> = agents
                .iter()
                .filter(|key| !available(key))
                .map(|key| crate::ui::keys::npub(key))
                .collect();
            if unavailable.is_empty() {
                Ok(())
            } else {
                Err((
                    3,
                    format!(
                        "Selected agents are unavailable in this community: {}. Refresh agents or remove them before creating the channel.",
                        unavailable.join(", ")
                    ),
                ))
            }
        })
    })
}

fn create_now(
    session: std::sync::Arc<buzz_relay_client::Session>,
    channel: NewChannel,
    setup: Option<buzz_relay_client::kit::setup::ChannelSetup>,
    cx: &mut App,
) -> Task<Result<Value, Failure>> {
    on_tokio(cx, async move {
        // As the New channel form does, so `channels/creation` shows it.
        let input = buzz_relay_client::kit::setup::ChannelCreationInput::from_new(&channel, setup);
        let id = session
            .create_channel_with(input)
            .await
            .map_err(|error| (3, error))?;
        Ok(json!({ "channel": id }))
    })
}

/// `channels/creation`: the channel being made now, if any, and notices
/// about setups that did not finish (`channelCreation.snapshot`).
pub fn creation(cx: &App) -> Result<Value, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    let snapshot = session.creation().borrow().clone();
    Ok(creation_json(snapshot.pending.as_ref(), &snapshot.notices))
}

/// `channels/creation` as a request: this session's creation, or else one
/// the outbox still holds from before a restart (`pendingCreation()`).
pub fn creation_now(cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let pending = session.pending_creation().await;
        let snapshot = session.creation().borrow().clone();
        Ok(creation_json(pending.as_ref(), &snapshot.notices))
    }))
}

fn creation_json(
    pending: Option<&buzz_relay_client::kit::setup::ChannelCreationInput>,
    notices: &[buzz_relay_client::kit::setup::SetupNotice],
) -> Value {
    json!({
        "pending": pending.map(|pending| json!({
            "name": pending.name,
            "description": pending.description,
            "visibility": format!("{:?}", pending.visibility).to_ascii_lowercase(),
            "ttlSeconds": pending.ttl_seconds,
            "labels": pending.labels,
            "setup": pending.setup.as_ref().map(|setup| json!({
                "canvas": setup.canvas,
                "agents": setup.agents,
                "group": setup.group_id,
                "template": setup.template_id,
            })),
        })),
        "notices": notices.iter().map(|notice| json!({
            "id": notice.id,
            "name": notice.name,
            "error": notice.error,
        })).collect::<Vec<_>>(),
    })
}

/// `channels/dismissNotice`: hides a setup notice.
pub fn dismiss_notice(params: &Value, cx: &App) -> Result<Value, Failure> {
    let id = text(params, "id").ok_or_else(|| invalid("id is required"))?;
    community(cx)?.read(cx).session.dismiss_setup_notice(id);
    Ok(Value::Null)
}

/// `channels/action`: joins, leaves, archives, unarchives or deletes a
/// channel, or hides a DM, and answers once the relay shows it.
pub fn action(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let action = match text(params, "action") {
        Some("join") => Action::Join,
        Some("leave") => Action::Leave,
        Some("archive") => Action::Archive,
        Some("unarchive") => Action::Unarchive,
        Some("delete") => Action::Delete,
        Some("hide") => Action::Hide,
        _ => {
            return Err(invalid(
                "action must be join, leave, archive, unarchive, delete or hide",
            ));
        }
    };
    // A channel to join need not be one of yours yet.
    let id = text(params, "channel")
        .ok_or_else(|| invalid("channel is required"))?
        .to_owned();
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .channel_action(action, &id)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error.to_string()))
    }))
}

/// `channels/addMember`: adds someone, such as one of your agents, to a
/// channel, and answers once the roster lists them.
pub fn add_member(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    // An open channel you have not joined too; the relay refuses one
    // you may not reach (`canReadRemote`).
    let channel = readable(params, community.read(cx))?;
    let pubkey = super::capabilities::pubkey(params, "pubkey")
        .ok_or_else(|| invalid("Choose a valid person or agent."))?
        .to_owned();
    // An archived identity is never added (`addChannelMember`).
    if crate::ui::archives::hides(&community, &pubkey, cx) {
        return Err((
            3,
            "Archived identities cannot be added. Choose someone else.".to_owned(),
        ));
    }
    // One of your agents gets the bot role even before its profile is
    // read (`addChannelMember`'s agent choices).
    let agent = crate::ui::me::recipients::rules(&community, cx)
        .selectable
        .contains(&pubkey);
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let added = if agent {
            session.add_agent_member(&channel, &pubkey).await
        } else {
            session.add_member(&channel, &pubkey).await
        };
        added.map(|()| Value::Null).map_err(|error| (3, error))
    }))
}

/// `channels/changeMember`: makes a member an admin, member or guest, or
/// removes them; `expected` is the role they have now.
pub fn change_member(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::lifecycle::MemberRole;
    use buzz_relay_client::members::{Change, MemberChange};
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let pubkey = text(params, "pubkey")
        .ok_or_else(|| invalid("pubkey is required"))?
        .to_ascii_lowercase();
    let change = match text(params, "change") {
        Some("admin") => Change::Admin,
        Some("member") => Change::Member,
        Some("guest") => Change::Guest,
        Some("remove") => Change::Remove,
        _ => return Err(invalid("change must be admin, member, guest or remove")),
    };
    let expected = match text(params, "expected") {
        Some("owner") => MemberRole::Owner,
        Some("admin") => MemberRole::Admin,
        Some("member") => MemberRole::Member,
        Some("guest") => MemberRole::Guest,
        Some("bot") => MemberRole::Bot,
        _ => return Err(invalid("expected must be the member's role now")),
    };
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .change_member(
                &channel,
                MemberChange {
                    pubkey,
                    expected,
                    change,
                },
            )
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `threads/follow`: follows or unfollows a thread on this device, or
/// with no `following`, says whether you do.
pub fn follow(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let root = event_id(params, "root")?;
    let following = params.get("following").and_then(Value::as_bool);
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        match following {
            None => Ok(json!(session.following(&channel, root).await)),
            Some(following) => session
                .follow(&channel, root, following)
                .await
                .map(|()| json!(following))
                .map_err(|error| (3, error)),
        }
    }))
}

fn reminder(reminder: &Reminder) -> Value {
    json!({
        "id": reminder.id,
        "eventId": reminder.event_id,
        "createdAt": reminder.created_at,
        "notBefore": reminder.not_before,
        "status": match reminder.status {
            ReminderStatus::Pending => "pending",
            ReminderStatus::Done => "done",
            ReminderStatus::Cancelled => "cancelled",
        },
        "note": reminder.note,
        "message": reminder.target.as_ref().map(|target| json!({
            "id": target.event_id,
            "channel": target.channel_id,
            "preview": target.preview,
            "author": target.author_pubkey,
        })),
    })
}

fn reminders_now(cx: &App) -> Result<Value, Failure> {
    let state = community(cx)?.read(cx).session.reminders().borrow().clone();
    Ok(json!({
        "status": format!("{:?}", state.status).to_ascii_lowercase(),
        "hydrated": state.hydrated,
        "reminders": state.reminders.iter().map(reminder).collect::<Vec<_>>(),
        "error": state.error,
    }))
}

/// `reminders/list`: your reminders.
pub fn list(cx: &App) -> Result<Value, Failure> {
    reminders_now(cx)
}

/// `reminders/watch`: sends `reminders/changed` with your reminders now
/// and whenever they change.
pub fn watch(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let community = community(cx)?;
    let mut receiver = community.read(cx).session.reminders();
    let (sender, mut changes) = futures::channel::mpsc::unbounded::<()>();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        while receiver.changed().await.is_ok() {
            if sender.unbounded_send(()).is_err() {
                return;
            }
        }
    });
    let first = reminders_now(cx)?;
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        cx.update(|cx| notify(first, cx));
        while changes.next().await.is_some() {
            cx.update(|cx| {
                if let Ok(now) = super::capabilities::scoped(&community, || reminders_now(cx)) {
                    notify(now, cx);
                }
            });
        }
    });
    Ok((Watch::new((), forward), json!({})))
}

/// `reminders/create`, `reminders/snooze`, `reminders/complete` and
/// `reminders/cancel`.
pub fn change(
    method: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let session = community.read(cx).session.clone();
    let at = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid(format!("{key} (unix seconds) is required")))
    };
    let id = || {
        text(params, "id")
            .map(str::to_owned)
            .ok_or_else(|| invalid("id is required"))
    };
    let task = match method {
        "reminders/create" => {
            // A message in an open channel you have not joined too.
            let channel = readable(params, community.read(cx))?;
            let message_id = event_id(params, "message")?;
            let not_before = at("notBefore")?;
            let note = text(params, "note").map(str::to_owned);
            let preview = text(params, "preview").unwrap_or_default().to_owned();
            let author = text(params, "author").unwrap_or_default().to_owned();
            let target = ReminderTarget {
                event_id: message_id.to_hex(),
                channel_id: channel,
                preview,
                author_pubkey: author,
            };
            on_tokio(cx, async move {
                session
                    .create_reminder(target, not_before, note)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        "reminders/snooze" => {
            let (id, until) = (id()?, at("notBefore")?);
            on_tokio(cx, async move {
                session
                    .snooze_reminder(&id, until)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        "reminders/complete" => {
            let id = id()?;
            on_tokio(cx, async move {
                session
                    .complete_reminder(&id)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        _ => {
            let id = id()?;
            on_tokio(cx, async move {
                session
                    .cancel_reminder(&id)
                    .await
                    .map_err(|error| (3, error))
            })
        }
    };
    Ok(cx.background_spawn(async move { task.await.map(|()| Value::Null) }))
}

/// `archives/get`: the identities the community archived, which Buzz
/// leaves out of mentions, member choices and search, once read;
/// `refresh: true` reads them again.
pub fn archives(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use crate::ui::archives::{self, Status};
    let community = community(cx)?;
    let again = params.get("refresh").and_then(Value::as_bool) == Some(true);
    archives::ensure(&community, again, cx);
    Ok(cx.spawn(async move |cx| {
        // The read takes a request or two; wait up to 15 seconds.
        for _ in 0..150 {
            let status = cx.update(|cx| archives::status(&community, cx));
            if status != Some(Status::Loading) {
                break;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
        // The read's own outcome, as `archives.snapshot()` gives it.
        Ok(cx.update(|cx| archives::snapshot_json(&community, cx)))
    }))
}

/// `archives/request`: archives or unarchives an identity in the
/// community (`archives.request`), with its owner attestation when it is
/// one of your agents, or your relay owner or admin role. Answers once
/// the archive shows it.
pub fn request_archive(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let archive = match text(params, "action") {
        Some("archive") => true,
        Some("unarchive") => false,
        _ => return Err(invalid("action must be archive or unarchive")),
    };
    let target = super::capabilities::pubkey(params, "pubkey")
        .ok_or_else(|| (3, "You can no longer archive this identity".to_owned()))?
        .to_owned();
    let owner = match &cx.global::<crate::community::AppState>().identity {
        crate::community::IdentityState::Ready(identity) => identity.clone(),
        _ => return Err((1, "You are not signed in.".to_owned())),
    };
    let community = community(cx)?;
    let session = community.read(cx).session.clone();
    let request = on_tokio(cx, async move {
        crate::agents2::cleanup::set_archived(&session, &owner, &target, archive)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    });
    // Then read the archive Buzz shows again (`request` ends with
    // `refresh()`), so its filters and `archives/get` see the change.
    Ok(cx.spawn(async move |cx| {
        use crate::ui::archives::{self, Status};
        let done = request.await;
        if done.is_ok() {
            cx.update(|cx| archives::ensure(&community, true, cx));
            // Answered once that read finishes (`await refresh()`).
            for _ in 0..150 {
                if cx.update(|cx| archives::status(&community, cx)) != Some(Status::Loading) {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
        }
        done
    }))
}

/// `archives/consent`: how you may archive or unarchive `pubkey` here
/// (`archives.consent`): `{"auth"}`, the owner attestation of one of your
/// agents, `{"admin": true}` by your relay role, or `null` when you may not.
pub fn archive_consent(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    // No way to archive a malformed key (`consent` answers `null`).
    let Some(target) = super::capabilities::pubkey(params, "pubkey").map(str::to_owned) else {
        return Ok(Task::ready(Ok(Value::Null)));
    };
    let owner = match &cx.global::<crate::community::AppState>().identity {
        crate::community::IdentityState::Ready(identity) => identity.clone(),
        _ => return Err((1, "You are not signed in.".to_owned())),
    };
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        crate::agents2::cleanup::archive_consent(&session, &owner, &target)
            .await
            .map_err(|error| (2, error))
    }))
}

/// `sessions/addAgents`: adds your `agents` to a channel or work session,
/// and to a session's parent first (`workSessions.addAgents`).
pub fn add_session_agents(
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let agents: Vec<String> = params
        .get("agents")
        .and_then(Value::as_array)
        .filter(|agents| !agents.is_empty() && agents.len() <= 32)
        .ok_or_else(|| invalid("agents must list 1 to 32 of your agents"))?
        .iter()
        .map(|key| {
            key.as_str()
                .filter(|key| {
                    key.len() == 64
                        && key
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                })
                .map(str::to_owned)
                .ok_or_else(|| invalid("Choose an agent from your agent library."))
        })
        .collect::<Result<_, _>>()?;
    // Your agents, or ones already in the parent channel: the session
    // checks that against a fresh roster (`addAgents`).
    let rules = crate::ui::me::recipients::rules(&community, cx);
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .add_agents(&channel, &agents, &rules)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `sessions/create`: a new private work session titled `title`, under
/// `parent` if given, with `agents` (your agents in this community)
/// added. Answers with its channel id once the relay lists it.
pub fn create_session(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::session::Expected;
    let title = text(params, "title")
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or_else(|| invalid("title is required"))?;
    let title = buzz_relay_client::session::session_title(title);
    let parent = text(params, "parent").map(str::to_owned);
    let agents: Vec<String> = params
        .get("agents")
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .map(|key| {
                    key.as_str()
                        .filter(|key| {
                            key.len() == 64
                                && key
                                    .bytes()
                                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                        })
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("Choose an agent from your agent library."))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let community = community(cx)?;
    let rules = crate::ui::me::recipients::rules(&community, cx);
    let session = community.read(cx).session.clone();
    let id = uuid::Uuid::new_v4().to_string();
    Ok(on_tokio(cx, async move {
        let fail = |error: String| (3, error);
        let created = session
            .work_session_create(&id, &title, parent.as_deref())
            .await
            .map_err(fail)?;
        session
            .work_session_delivered(created)
            .await
            .map_err(fail)?;
        session
            .work_session_refresh(&id, Expected::default(), true)
            .await
            .map_err(fail)?;
        if !agents.is_empty() {
            session
                .add_agents(&id, &agents, &rules)
                .await
                .map_err(fail)?;
        }
        Ok(json!({ "channel": id }))
    }))
}

/// `sessions/memberOf`: the channels that list `pubkey` as a member.
pub fn member_of(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    // As given, as `memberChannels` reads `#p`: a key no channel lists
    // finds none.
    let pubkey = text(params, "pubkey")
        .ok_or_else(|| invalid("pubkey is required"))?
        .to_owned();
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .member_channels(&pubkey)
            .await
            .map(|channels| json!(channels))
            .map_err(|error| (2, error))
    }))
}

/// `sidebar/get`: how the user arranged the sidebar: starred and muted
/// channels, saved groups and where channels sit in them, and each
/// group's sort (`sidebarPreferences`).
pub fn sidebar(cx: &App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let state = community.read(cx).snapshot.sidebar.clone();
    Ok(sidebar_json(&state))
}

/// A sidebar (Messages or Me) as `sidebar/get` and `me/get` give it.
fn sidebar_json(state: &buzz_relay_client::sidebar_store::SidebarState) -> Value {
    use buzz_relay_client::sidebar_store::{Destination, PreferencesStatus};
    let (status, error) = match &state.status {
        PreferencesStatus::Idle => ("idle", None),
        PreferencesStatus::Loading => ("loading", None),
        PreferencesStatus::Ready => ("ready", None),
        PreferencesStatus::Error(error) => ("error", Some(error.clone())),
    };
    let shared = json!({
        "status": status,
        "error": error,
        "cached": state.cached,
        "writable": state.writable,
        // Stars and moves save with the same record.
        "starWritable": state.writable,
        "sortWritable": state.sort_writable(),
        "muteWritable": state.mute_writable(),
        "removeGroupWritable": state.remove_section_writable(),
        "sortErrors": state.sort_errors.iter().map(|failure| json!({
            "group": failure.group,
            "mode": failure.mode.as_str(),
            "error": failure.error,
        })).collect::<Vec<_>>(),
        "moves": state.moves.iter().map(|change| json!({
            "channel": change.intent.channel_id,
            "to": match &change.intent.destination {
                Destination::Starred => json!("starred"),
                Destination::Section(None) => json!("default"),
                Destination::Section(Some(id)) => json!(format!("section:{id}")),
                Destination::Create(section) => json!({ "new": section.name }),
            },
            "pending": change.pending,
            "error": change.error,
        })).collect::<Vec<_>>(),
        "hiddenDms": state.hidden_dms,
        "meChannels": state.me_channels,
    });
    let Some(data) = &state.data else {
        let mut answer = shared;
        answer["loaded"] = json!(false);
        return answer;
    };
    let mut answer = json!({
        "loaded": true,
        "starred": data.starred,
        "muted": data.muted,
        "sort": data
            .sort
            .iter()
            .map(|(group, mode)| (group.clone(), json!(mode.as_str())))
            .collect::<serde_json::Map<_, _>>(),
        "groups": data.sections.iter().map(|section| json!({
            "id": section.id,
            "name": section.name,
            "icon": section.icon,
            "order": section.order,
        })).collect::<Vec<_>>(),
        "assignments": data.assignments,
    });
    if let (Value::Object(answer), Value::Object(shared)) = (&mut answer, shared) {
        answer.extend(shared);
    }
    answer
}

/// `me/get`: your Me groups (`mePreferences`): groups, which group each
/// Me conversation is in, moves still saving or failed, and what can be
/// saved, as `sidebar/get` gives the Messages sidebar.
pub fn me(cx: &App) -> Result<Value, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    session.me_kit().me_ensure();
    let mut me = sidebar_json(&session.me_kit().me_state());
    // Me holds groups only: nothing there is starred, sorted or muted.
    for flag in ["starWritable", "sortWritable", "muteWritable"] {
        me[flag] = json!(false);
    }
    // The conversations in Me, from the placement record.
    me["meChannels"] = json!(session.me_kit().snapshot().me_channels());
    Ok(me)
}

/// `me/assign`, `me/retryMove`, `me/removeGroup`: moves a conversation
/// into a Me group (`group`, a `newGroup` name, or `null` for
/// Conversations), tries a failed move again, or deletes a group.
pub fn change_me(
    method: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::sidebar::NewSection;
    let session = community(cx)?.read(cx).session.clone();
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let method = method.to_owned();
    let channel = text("channel");
    let group = text("group");
    let new_group = text("newGroup");
    if method != "me/removeGroup" && channel.is_none() {
        return Err(invalid("channel is required"));
    }
    if method == "me/removeGroup" && group.is_none() {
        return Err(invalid("group is required"));
    }
    Ok(on_tokio(cx, async move {
        let channel = channel.unwrap_or_default();
        let done = match method.as_str() {
            "me/retryMove" => session.me_kit().me_retry_move(&channel).await,
            "me/removeGroup" => {
                session
                    .me_kit()
                    .me_remove_section(&group.unwrap_or_default())
                    .await
            }
            _ => match new_group {
                Some(name) => {
                    session
                        .me_kit()
                        .me_create_and_assign(
                            &channel,
                            NewSection {
                                id: uuid::Uuid::new_v4().to_string(),
                                name,
                            },
                        )
                        .await
                }
                None => session.me_kit().me_assign(&channel, group).await,
            },
        };
        done.map(|()| Value::Null).map_err(|error| (3, error))
    }))
}

/// `me/place`: puts a conversation in Me, optionally in a `group`, or
/// takes it out with `me: false` (`mePlacement.set`).
pub fn place_me(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    // Any channel id, such as a session just made or one you left.
    let channel = readable(params, community.read(cx))?;
    let personal = params.get("me").and_then(Value::as_bool).unwrap_or(true);
    let group = params
        .get("group")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kit = community.read(cx).session.me_kit().clone();
    Ok(on_tokio(cx, async move {
        kit.set_placement(&channel, personal, group.as_deref())
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `me/admit`: whether a new conversation would fit in Me, in `group`
/// if given (`mePlacement.admit`): `null`, or an error saying why not.
pub fn admit_me(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    // Any channel id, as the Me placement admits one it has not seen.
    let channel = readable(params, community.read(cx))?;
    let group = params
        .get("group")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kit = community.read(cx).session.me_kit().clone();
    Ok(on_tokio(cx, async move {
        kit.admit(&channel, group.as_deref())
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `me/dismiss`: forgets a failed Me move; `me/refresh` reads the groups
/// again.
pub fn me_local(method: &str, params: &Value, cx: &App) -> Result<Value, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    if method == "me/refresh" {
        session.me_kit().me_refresh();
        return Ok(Value::Null);
    }
    let channel = params
        .get("channel")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("channel is required"))?;
    session.me_kit().me_dismiss_move(channel);
    Ok(Value::Null)
}

/// `sidebar/star`, `sidebar/mute` and `sidebar/sort`: stars or unstars
/// a channel, mutes or unmutes it, or sorts a group (`starred`,
/// `channels`, `forums`, `dms` or `section:<id>`) by `alpha` or `recent`.
pub fn change_sidebar(
    method: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::sidebar::SortMode;
    use buzz_relay_client::sidebar_store::Destination;
    let session = community(cx)?.read(cx).session.clone();
    let on = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid(format!("{key} must be true or false")))
    };
    let task = match method {
        "sidebar/sort" => {
            let group = text(params, "group")
                .ok_or_else(|| invalid("group is required"))?
                .to_owned();
            let mode = match text(params, "mode") {
                Some("alpha") => SortMode::Alpha,
                Some("recent") => SortMode::Recent,
                _ => return Err(invalid("mode must be alpha or recent")),
            };
            on_tokio(cx, async move {
                session
                    .sidebar_sort(&group, mode)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        "sidebar/mute" => {
            let channel = channel(params, community(cx)?.read(cx))?;
            let muted = on("muted")?;
            on_tokio(cx, async move {
                session
                    .sidebar_mute(&channel, muted)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        "sidebar/removeGroup" => {
            let group = text(params, "group")
                .ok_or_else(|| invalid("group is required"))?
                .to_owned();
            on_tokio(cx, async move {
                session
                    .sidebar_remove_section(&group)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        "sidebar/assign" => {
            let channel = channel(params, community(cx)?.read(cx))?;
            // A saved group, a new one by `newGroup` name, or neither
            // for the channel's default section.
            let destination = match (text(params, "group"), text(params, "newGroup")) {
                (Some(group), _) => Destination::Section(Some(group.to_owned())),
                // A blank name or one past 256 is refused, as buzz-app's
                // `validSidebarAssignment` does.
                (None, Some(name))
                    if name.trim().is_empty() || name.encode_utf16().count() > 256 =>
                {
                    return Err(invalid("Invalid sidebar assignment intent"));
                }
                (None, Some(name)) => Destination::Create(buzz_relay_client::sidebar::NewSection {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.trim().to_owned(),
                }),
                _ => Destination::Section(None),
            };
            on_tokio(cx, async move {
                session
                    .sidebar_move(&channel, destination)
                    .await
                    .map_err(|error| (3, error))
            })
        }
        _ => {
            let channel = channel(params, community(cx)?.read(cx))?;
            let destination = if on("starred")? {
                Destination::Starred
            } else {
                Destination::Section(None)
            };
            on_tokio(cx, async move {
                session
                    .sidebar_move(&channel, destination)
                    .await
                    .map_err(|error| (3, error))
            })
        }
    };
    Ok(cx.background_spawn(async move { task.await.map(|()| Value::Null) }))
}

fn role_name(role: buzz_relay_client::lifecycle::MemberRole) -> &'static str {
    use buzz_relay_client::lifecycle::MemberRole;
    match role {
        MemberRole::Owner => "owner",
        MemberRole::Admin => "admin",
        MemberRole::Member => "member",
        MemberRole::Guest => "guest",
        MemberRole::Bot => "bot",
        MemberRole::Unknown => "unknown",
    }
}

/// `channels/members`: the channel's members with their roles, read
/// fresh from the relay-signed roster, and what you may do to each
/// (`memberAdministration`): change their role (`canManage`) or remove
/// them, which you may also do to your own agents (`canRemove`).
pub fn members(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    // An open channel you have not joined too; the relay refuses one
    // you may not reach (`canReadRemote`).
    let channel = readable(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    let viewer = session.viewer().to_string();
    Ok(on_tokio(cx, async move {
        // A fresh read settles an uncertain change the roster now shows
        // (`administration.refresh`), so it stops blocking the next.
        session
            .refresh_members(&channel)
            .await
            .map_err(|error| (2, error))?;
        let state = session
            .channel_state(&channel)
            .await
            .map_err(|error| (2, error))?;
        let operation = session.member_operation(&channel).await.map(|operation| {
            use buzz_relay_client::MemberOperationStatus as Status;
            use buzz_relay_client::members::Change;
            json!({
                "pubkey": operation.change.pubkey,
                "change": match operation.change.change {
                    Change::Admin => "admin",
                    Change::Member => "member",
                    Change::Guest => "guest",
                    Change::Remove => "remove",
                },
                "status": match operation.status {
                    Status::Pending => "pending",
                    Status::Uncertain => "uncertain",
                    Status::Confirmed => "confirmed",
                    Status::Failed => "failed",
                },
                "error": operation.error,
            })
        });
        // Your agents are known by their profiles' owners, so the
        // profiles of members you cannot otherwise remove are read first.
        let unknown: Vec<String> = {
            let snapshot = session.snapshot().borrow().clone();
            state
                .roles
                .keys()
                .filter(|key| {
                    !state.can_remove_member(&viewer, key, None)
                        && !snapshot.profiles.contains_key(*key)
                })
                .cloned()
                .collect()
        };
        if !unknown.is_empty() {
            let _ = session.load_profiles(unknown).await;
        }
        let profiles = session.snapshot().borrow().profiles.clone();
        let members: Vec<Value> = state
            .roles
            .iter()
            .map(|(key, role)| {
                let owner = profiles
                    .get(key)
                    .and_then(|profile| profile.owner_pubkey.clone());
                json!({
                    "pubkey": key,
                    "role": role_name(*role),
                    "canManage": state.can_manage_member(&viewer, key),
                    "canRemove": state.can_remove_member(&viewer, key, owner.as_deref()),
                })
            })
            .collect();
        Ok(json!({
            "channel": channel,
            "version": state.version.to_hex(),
            "canManage": state.can_manage(&viewer),
            "members": members,
            "operation": operation,
        }))
    }))
}

/// `channels/lifecycle`: what you may do to the channel as a whole
/// (`channelLifecycle.load`): archive, unarchive, delete, leave or hide
/// it, and why leaving is not offered.
pub fn lifecycle(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        // With what an owner agent of yours grants, as the channel menu.
        let (state, can) = futures::join!(
            session.channel_state(&channel),
            session.channel_permissions(&channel)
        );
        let (state, can) = (
            state.map_err(|error| (2, error))?,
            can.map_err(|error| (2, error))?,
        );
        Ok(json!({
            "channel": channel,
            "channelType": format!("{:?}", state.channel_type).to_ascii_lowercase(),
            "canArchive": can.can_archive,
            "canUnarchive": can.can_unarchive,
            "canDelete": can.can_delete,
            "canLeave": can.can_leave,
            "canHide": can.can_hide,
            "leaveReason": can.leave_reason,
            "deleteUnavailable": can.delete_unavailable,
        }))
    }))
}

/// `community/refresh`: reads one of the community's views again, as
/// its Retry would: `unread`, `inbox`, `reminders`, `emoji`, `agents` or
/// `kit`.
pub fn refresh(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let session = community.read(cx).session.clone();
    let what = params
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match what.as_str() {
        "unread" => session.refresh_unread(),
        "inbox" => session.refresh_inbox(),
        "reminders" => session.refresh_reminders(),
        "emoji" => session.refresh_emoji(),
        "hiddenDms" => session.refresh_dm_visibility(),
        "sidebar" => session.refresh_sidebar(),
        "channels" => session.refresh_channels(),
        "roster" => session.refresh_roster(),
        // Your agent list, read again even once read, and the archive if
        // it was read (`agentChoices.refresh`: archive reads stay lazy).
        "agents" => {
            let origin = community.read(cx).origin.http();
            // The hosts' lists too (`native?.refresh()`).
            super::agents::agents2(cx)?.update(cx, |agents, cx| {
                agents.refresh_listings(&origin, cx);
                agents.refresh(cx).detach();
            });
            if crate::ui::archives::status(&community, cx).is_some() {
                crate::ui::archives::ensure(&community, true, cx);
            }
        }
        "kit" => {
            return Ok(on_tokio(cx, async move {
                session
                    .kit_refresh()
                    .await
                    .map(|()| Value::Null)
                    .map_err(|error| (3, error))
            }));
        }
        _ => {
            return Err((
                INVALID_PARAMS,
                "what is unread, inbox, reminders, emoji, hiddenDms, sidebar, channels, roster, agents or kit"
                    .to_owned(),
            ));
        }
    }
    Ok(Task::ready(Ok(Value::Null)))
}

/// `sidebar/retryMove`: tries a channel's failed move between sections
/// again.
pub fn retry_move(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .sidebar_retry_move(&channel)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `sidebar/dismiss`: forgets a failed move (`channel`) or sort
/// (`group`) the sidebar shows an error for.
pub fn dismiss_sidebar(params: &Value, cx: &App) -> Result<Value, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    match (
        params.get("channel").and_then(Value::as_str),
        params.get("group").and_then(Value::as_str),
    ) {
        (Some(channel), _) => session.sidebar_dismiss_move(channel),
        (None, Some(group)) => session.sidebar_dismiss_sort(group),
        (None, None) => {
            return Err((INVALID_PARAMS, "channel or group is required".to_owned()));
        }
    }
    Ok(Value::Null)
}
