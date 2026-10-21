//! What is unread, and marking it read (buzz-app's `UnreadCapability`):
//! the selected community's unread counts, followed live, each channel's
//! unread threads, the inbox, and read marks as the user.

use super::capabilities::{Failure, Watch, channel, community, on_tokio};
use super::protocol::INVALID_PARAMS;
use buzz_relay_client::unread::ThreadActivitySnapshot;
use buzz_relay_client::{EventId, UnreadCounts};
use gpui_kit::*;
use serde_json::{Value, json};

fn invalid(message: impl Into<String>) -> Failure {
    (INVALID_PARAMS, message.into())
}

/// Unread counts as plugins see them.
fn counts(unread: &UnreadCounts) -> Value {
    let channels: serde_json::Map<String, Value> = unread
        .channels
        .iter()
        .map(|(id, channel)| {
            (
                id.clone(),
                json!({
                    "unread": channel.unread,
                    "count": channel.count,
                    "attention": channel.attention,
                    "mentions": channel.mentions,
                    "threads": channel.threads,
                    "manual": channel.manual != Default::default(),
                    // Which mark: this device's, or one synced from another.
                    "manualKind": manual(channel.manual),
                    "latestAt": unread.latest.get(id),
                    "preview": unread.previews.get(id),
                    "stale": channel.stale,
                    "threadsStale": channel.threads_stale,
                }),
            )
        })
        .collect();
    json!({
        "channels": channels,
        "mentions": unread.mentions,
        "direct": unread.direct,
        "observed": unread.observed,
    })
}

/// `unread/get`: what is unread in the selected community. Channels with
/// nothing unread are left out.
pub fn get(cx: &App) -> Result<Value, Failure> {
    let community = community(cx)?;
    Ok(counts(&community.read(cx).snapshot.unread))
}

/// `unread/watch`: sends `unread/changed` with the counts now and whenever
/// they change.
pub fn watch(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let community = community(cx)?;
    let first = counts(&community.read(cx).snapshot.unread);
    // What was last sent: the counts carry more than plugins see, so
    // compare what they would get.
    let last = std::cell::RefCell::new(first.clone());
    let notify = std::rc::Rc::new(notify);
    let send = notify.clone();
    cx.defer(move |cx| send(first, cx));
    let subscription = cx.observe(&community, move |community, cx| {
        let unread = counts(&community.read(cx).snapshot.unread);
        if unread == *last.borrow() {
            return;
        }
        *last.borrow_mut() = unread.clone();
        notify(unread, cx);
    });
    Ok((Watch::new(subscription, Task::ready(())), json!({})))
}

pub(super) fn activity(snapshot: &ThreadActivitySnapshot) -> Value {
    json!({
        "channel": snapshot.channel_id,
        "items": snapshot.items.as_ref().map(|items| {
            items
                .iter()
                .map(|item| json!({
                    "root": item.root_id,
                    "latestMessage": item.latest_message_id,
                    "author": item.author_id,
                    "workflowOwner": item.workflow_owner_id,
                    "createdAt": item.created_at,
                    "preview": item.preview,
                    "unreadCount": item.unread_count,
                }))
                .collect::<Vec<_>>()
        }),
        "coverage": format!("{:?}", snapshot.coverage).to_ascii_lowercase(),
        "freshness": format!("{:?}", snapshot.freshness).to_ascii_lowercase(),
        "error": snapshot.error,
    })
}

/// What `threads/activity` gives before read state loads: nothing known.
pub(super) fn no_activity(channel: &str) -> Value {
    json!({
        "channel": channel,
        "items": null,
        "coverage": "unknown",
        "freshness": "unknown",
        "error": null,
    })
}

/// `threads/activity`: a channel's threads with unread replies for the
/// user, newest first.
pub fn threads(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        match session.thread_activity(&channel).await {
            Some(snapshot) => Ok(activity(&snapshot)),
            None => Ok(no_activity(&channel)),
        }
    }))
}

/// `unread/sync`: how read state is syncing to the relay, and
/// `unread/retrySync` to try again after a failure.
pub fn sync(retry: bool, cx: &App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let community = community.read(cx);
    if retry {
        community.session.retry_read_sync();
        return Ok(Value::Null);
    }
    Ok(match &community.snapshot.unread.sync {
        None => Value::Null,
        Some(sync) => {
            use buzz_relay_client::read_state::ReadCapability;
            let completeness = format!("{:?}", sync.completeness).to_ascii_lowercase();
            json!({
                "capability": match sync.capability {
                    ReadCapability::Unsupported => "unsupported",
                    ReadCapability::ReadOnly => "read-only",
                    ReadCapability::FrontierSync => "frontier-sync",
                },
                "status": format!("{:?}", sync.status).to_ascii_lowercase(),
                "completeness": completeness,
                // The name this answer first used.
                "complete": completeness,
                "error": sync.error,
            })
        }
    })
}

/// `unread/target`: the unread state of a channel, or of a `thread` or
/// `message` in it (`unread.snapshot(target)`), or `null` before read
/// state loads. Counts are `null` without evidence, never a made-up zero.
pub fn target(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::read_state::ReadTarget;
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let id = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(|hex| {
                EventId::from_hex(hex)
                    .map(|id| id.to_hex())
                    .map_err(|_| invalid(format!("{key} is not an event id")))
            })
            .transpose()
    };
    let target = match (id("thread")?, id("message")?) {
        (Some(root), None) => ReadTarget::thread(&channel, root),
        (None, Some(message)) => ReadTarget::message(&channel, message),
        (None, None) => ReadTarget::channel(&channel),
        (Some(_), Some(_)) => return Err(invalid("name a thread or a message, not both")),
    };
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let Some(snapshot) = session.unread_snapshot(target).await else {
            return Ok(Value::Null);
        };
        Ok(json!({
            "channel": channel,
            "latestMessage": snapshot.latest_message.as_ref().map(|latest| json!({
                "id": latest.id,
                "createdAt": latest.created_at,
                "preview": latest.preview,
            })),
            "observedCount": snapshot.observed_count,
            "attentionCount": snapshot.attention_count,
            "mentionCount": snapshot.mention_count,
            "coverage": format!("{:?}", snapshot.coverage).to_ascii_lowercase(),
            "freshness": format!("{:?}", snapshot.freshness).to_ascii_lowercase(),
            "manual": manual(snapshot.manual),
            "error": snapshot.error,
        }))
    }))
}

/// `unread/message`: whether a loaded message shows as unread now.
pub fn message(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let id = params
        .get("message")
        .and_then(Value::as_str)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or_else(|| invalid("message is not an event id"))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        Ok(json!(session.message_unread(&channel, id).await))
    }))
}

/// `unread/attention`: a loaded message's attention as the badges see
/// it (`unread.attention`), or `null` before read state loads.
pub fn attention(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::unread::{AttentionStatus, Category};
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let id = params
        .get("message")
        .and_then(Value::as_str)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or_else(|| invalid("message is not an event id"))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let Some(attention) = session.message_attention(&channel, id).await else {
            return Ok(Value::Null);
        };
        Ok(json!({
            "status": match attention.status {
                AttentionStatus::Unknown => "unknown",
                AttentionStatus::Ineligible => "ineligible",
                AttentionStatus::Eligible => "eligible",
            },
            "category": attention.category.map(|category| match category {
                Category::Mention => "mention",
                Category::Direct => "direct",
                Category::Thread => "thread",
            }),
            "mentioned": attention.mentioned,
            "root": attention.root_id,
            "pending": attention.pending,
            "unread": attention.unread,
            "forced": attention.forced,
            "viewing": attention.viewing,
        }))
    }))
}

/// `read/markLocal` and `read/clearLocal`: marks a channel, a thread
/// (`thread`, its root) or a message unread on this device only, or
/// drops that mark (`markUnreadLocal`, `clearUnreadLocal`).
pub fn local(
    clear: bool,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::read_state::ReadTarget;
    let community = community(cx)?;
    let channel_id = channel(params, community.read(cx))?;
    let id = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(|hex| {
                EventId::from_hex(hex)
                    .map(|id| id.to_hex())
                    .map_err(|_| invalid(format!("{key} is not an event id")))
            })
            .transpose()
    };
    let target = match (id("thread")?, id("message")?) {
        (Some(root_id), _) => ReadTarget::Thread {
            channel_id,
            root_id,
        },
        (None, Some(message_id)) => ReadTarget::Message {
            channel_id,
            message_id,
        },
        (None, None) => ReadTarget::Channel { channel_id },
    };
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .mark_local(target, clear)
            .await
            .map(|()| saved(false))
            .map_err(|error| (3, error))
    }))
}

/// `inbox/get`: the inbox's conversations, as the Inbox page lists them,
/// built now if nothing followed them yet.
pub fn inbox(cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let receiver = session.inbox_now().await.map_err(|error| (3, error))?;
        let state = receiver.borrow().clone();
        Ok(inbox_json(&state))
    }))
}

fn inbox_json(state: &buzz_relay_client::session::InboxState) -> Value {
    use buzz_relay_client::session::InboxFeedStatus;
    use buzz_relay_client::unread::InboxStatus;
    json!({
        "status": match state.inbox.status {
            InboxStatus::Idle => "idle",
            InboxStatus::Loading => "loading",
            InboxStatus::Ready => "ready",
            InboxStatus::Error => "error",
        },
        "freshness": format!("{:?}", state.inbox.freshness).to_ascii_lowercase(),
        "items": state.inbox.items.iter().map(|item| json!({
            "id": item.id,
            "channel": item.channel_id,
            "target": read_target(&item.target),
            "message": item.message_id,
            "latestMessage": item.latest_message_id,
            "messageIds": item.message_ids,
            "messages": item.messages.iter().map(|message| json!({
                "id": message.id,
                "createdAt": message.created_at,
                "mentioned": message.mentioned,
            })).collect::<Vec<_>>(),
            "root": item.root_id,
            "author": item.author_id,
            "workflowOwner": item.workflow_owner_id,
            "preview": item.preview,
            "createdAt": item.created_at,
            "mentioned": item.mentioned,
            "thread": item.thread,
            "unreadCount": item.unread_count,
            // Marked unread on this device (`read/markLocal`).
            "manual": item.manual,
            // What marking it read marks (`readThrough`).
            "readThrough": item.read_through.iter().map(|through| json!({
                "target": read_target(&through.target),
                "message": through.message_id,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "error": state.inbox.error,
        "feed": {
            "status": match state.feed.status {
                InboxFeedStatus::Idle => "idle",
                InboxFeedStatus::Loading => "loading",
                InboxFeedStatus::Ready => "ready",
                InboxFeedStatus::Error => "error",
            },
            "error": state.feed.error,
            "incomplete": state.feed.incomplete,
        },
        "canRead": state.can_read,
    })
}

/// A read mark's place, in `read/mark`'s terms.
fn read_target(target: &buzz_relay_client::read_state::ReadTarget) -> Value {
    use buzz_relay_client::read_state::ReadTarget;
    match target {
        ReadTarget::Channel { channel_id } => json!({ "channel": channel_id }),
        ReadTarget::Thread {
            channel_id,
            root_id,
        } => json!({ "channel": channel_id, "thread": root_id }),
        ReadTarget::Message {
            channel_id,
            message_id,
        } => json!({ "channel": channel_id, "message": message_id }),
    }
}

/// `inbox/watch`: sends `inbox/changed` with what `inbox/get` gives,
/// now and whenever the Inbox changes.
pub fn watch_inbox(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let session = community(cx)?.read(cx).session.clone();
    let (sender, mut changes) = futures::channel::mpsc::unbounded::<Value>();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        // First the inbox as built now, not the state before it.
        let Ok(mut receiver) = session.inbox_now().await else {
            return;
        };
        let mut last = Value::Null;
        loop {
            let now = inbox_json(&receiver.borrow_and_update());
            if now != last {
                if sender.unbounded_send(now.clone()).is_err() {
                    return;
                }
                last = now;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    });
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(inbox) = changes.next().await {
            cx.update(|cx| notify(inbox, cx));
        }
    });
    Ok((Watch::new((), forward), json!({})))
}

fn manual(manual: buzz_relay_client::unread::Manual) -> &'static str {
    match manual {
        buzz_relay_client::unread::Manual::None => "none",
        buzz_relay_client::unread::Manual::LocalOnly => "local-only",
        buzz_relay_client::unread::Manual::Remote => "remote",
    }
}

/// `unread/watchTargets`: sends `unread/targetsChanged` with the unread
/// state of some threads and messages of one channel, as a view showing
/// them sees it (`unread.snapshot` and `subscribe` for thread and
/// message targets), now and whenever it changes.
pub fn watch_targets(
    params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    const MAX_TARGETS: usize = 500;
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let ids = |key: &str| -> Result<Vec<EventId>, Failure> {
        let Some(list) = params.get(key).filter(|list| !list.is_null()) else {
            return Ok(Vec::new());
        };
        let list = list
            .as_array()
            .filter(|list| list.len() <= MAX_TARGETS)
            .ok_or_else(|| invalid(format!("{key} is a list of at most {MAX_TARGETS} ids")))?;
        list.iter()
            .map(|id| {
                id.as_str()
                    .and_then(|hex| EventId::from_hex(hex).ok())
                    .ok_or_else(|| invalid(format!("{key} holds event ids")))
            })
            .collect()
    };
    let (threads, messages) = (ids("threads")?, ids("messages")?);
    let watch = community.read(cx).session.watch_unread(&channel);
    watch.set(threads, messages);
    let mut receiver = watch.snapshot.clone();
    let (sender, mut changes) = futures::channel::mpsc::unbounded::<Value>();
    let json_channel = channel.clone();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        loop {
            let detail = receiver.borrow_and_update().clone();
            let mut unread: Vec<String> = detail.unread.iter().map(EventId::to_hex).collect();
            let mut forced: Vec<String> = detail.forced.iter().map(EventId::to_hex).collect();
            unread.sort();
            forced.sort();
            let threads: serde_json::Map<String, Value> = detail
                .threads
                .iter()
                .map(|(root, thread)| {
                    (
                        root.to_hex(),
                        json!({
                            "manual": manual(thread.manual),
                            "observed": thread.observed,
                            "stale": thread.stale,
                        }),
                    )
                })
                .collect();
            let value = json!({
                "channel": json_channel,
                "threads": threads,
                "unread": unread,
                "forced": forced,
            });
            if sender.unbounded_send(value).is_err() || receiver.changed().await.is_err() {
                return;
            }
        }
    });
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(value) = changes.next().await {
            cx.update(|cx| notify(value, cx));
        }
    });
    Ok((Watch::new(watch, forward), json!({ "channel": channel })))
}

/// What a saved read mark answers (`ReadMutationResult`): it is saved
/// on this device, and goes to the relay when this connection syncs read
/// state (`pending`) or stays here (`local-only`). Local marks never sync.
fn saved(syncs: bool) -> Value {
    json!({
        "durability": "saved",
        "sync": if syncs { "pending" } else { "local-only" },
    })
}

/// Whether read marks sync to the relay on this connection.
fn syncs(cx: &App) -> bool {
    use buzz_relay_client::read_state::ReadCapability;
    community(cx).is_ok_and(|community| {
        community
            .read(cx)
            .snapshot
            .unread
            .sync
            .as_ref()
            .is_some_and(|sync| sync.capability == ReadCapability::FrontierSync)
    })
}

/// `read/mark`: marks everything (`all`), a channel, a message, or a
/// channel or thread up to a message (`through`) read, or a channel or
/// message unread (`read: false`).
pub fn mark(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let session = community.read(cx).session.clone();
    let syncs = syncs(cx);
    let id = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(|hex| {
                EventId::from_hex(hex).map_err(|_| invalid(format!("{key} is not an event id")))
            })
            .transpose()
    };
    let read = params.get("read").and_then(Value::as_bool).unwrap_or(true);
    if params.get("all").and_then(Value::as_bool) == Some(true) {
        if !read {
            return Err(invalid("all marks read only"));
        }
        // One result per channel it marked (`markAllChannelsRead`).
        return Ok(on_tokio(cx, async move {
            let marked = session.mark_all_read().await.map_err(|error| (3, error))?;
            Ok(Value::Array(
                marked
                    .into_iter()
                    .map(|(channel, result)| {
                        use buzz_relay_client::read_state::SyncKind;
                        let mut answer = saved(result.sync == SyncKind::Pending);
                        answer["channel"] = json!(channel);
                        answer
                    })
                    .collect(),
            ))
        }));
    }
    let channel = channel(params, community.read(cx))?;
    let message = id("message")?;
    let root = id("root")?;
    let through = params
        .get("through")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A thread is marked up to a message; without one this would mark
    // the whole channel.
    if (through || root.is_some()) && message.is_none() {
        return Err(invalid("through and root take a message"));
    }
    Ok(on_tokio(cx, async move {
        let done = match (message, read, through) {
            (None, true, _) => session.mark_channel_read(&channel).await,
            (None, false, _) => session.mark_channel_unread(&channel).await,
            (Some(message), true, true) => session.mark_through(&channel, root, message).await,
            (Some(message), true, false) => session.mark_message_read(&channel, message).await,
            (Some(message), false, _) => session.mark_message_unread(&channel, message).await,
        };
        // Unread marks stay on this device.
        done.map(|()| saved(syncs && read))
            .map_err(|error| (3, error))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn counts_list_unread_channels_and_totals() {
        let mut unread = UnreadCounts {
            mentions: 2,
            observed: true,
            ..UnreadCounts::default()
        };
        unread.channels.insert(
            "c1".into(),
            buzz_relay_client::ChannelUnread {
                unread: true,
                count: 3,
                mentions: 2,
                ..Default::default()
            },
        );
        let value = counts(&unread);
        assert_eq!(value["mentions"], 2);
        assert_eq!(value["channels"]["c1"]["count"], 3);
        assert_eq!(value["channels"]["c1"]["manual"], false);
    }
}
