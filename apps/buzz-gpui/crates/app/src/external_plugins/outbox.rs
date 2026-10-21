//! What the user's events are doing on the way to the relay (buzz-app's
//! `outbox`): every event queued for sending, such as messages, until the
//! relay shows it back. An event missing from the queue was delivered or
//! dismissed.

use super::capabilities::{Failure, Watch, community, on_tokio};
use super::protocol::INVALID_PARAMS;
use buzz_relay_client::EventId;
use buzz_relay_client::outbox::{Delivery, Operation, Outbox};
use gpui_kit::*;
use serde_json::{Value, json};

pub(super) fn delivery_name(delivery: Delivery) -> &'static str {
    self::delivery(delivery)
}

fn delivery(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Sending => "sending",
        Delivery::Accepted => "accepted",
        Delivery::Unknown => "unknown",
        Delivery::Failed => "failed",
        Delivery::Seen => "seen",
    }
}

fn operation(operation: &Operation) -> Value {
    let channel = operation
        .event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .find(|tag| tag.first().is_some_and(|name| name == "h"))
        .and_then(|tag| tag.get(1).cloned());
    json!({
        "id": operation.id().to_hex(),
        "kind": operation.event.kind.as_u16(),
        "channel": channel,
        "delivery": delivery(operation.delivery),
        "error": operation.error,
        "acknowledged": operation.acknowledged,
        "createdAt": operation.event.created_at.as_secs(),
        "content": operation.event.content,
        "tags": operation.event.tags.iter().map(|tag| tag.as_slice()).collect::<Vec<_>>(),
        // The signed event, once signed (`OutgoingEvent.signed`).
        "pubkey": operation.signed.as_ref().map(|signed| signed.pubkey.to_hex()),
        "sig": operation.signed.as_ref().map(|signed| signed.sig.to_string()),
    })
}

/// `outbox/send`: queues an event of yours as Buzz's own writes are
/// (`outbox.send`): signed once, retried, and followed by `outbox/get`
/// and `outbox/watch` until the relay shows it back. Answers its `id`.
pub fn send(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    use buzz_relay_client::messages::Draft;
    let kind = params
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|kind| u16::try_from(kind).ok())
        .ok_or((INVALID_PARAMS, "kind is required".to_owned()))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tags: Vec<Vec<String>> = match params.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(tags) => serde_json::from_value(tags.clone()).map_err(|_| {
            (
                INVALID_PARAMS,
                "tags must be a list of lists of strings".to_owned(),
            )
        })?,
    };
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let outbox = outbox_of(session).await?;
        outbox
            .send(
                Draft {
                    kind,
                    content,
                    tags,
                },
                Default::default(),
            )
            .map(|id| json!({ "id": id.to_hex() }))
            .map_err(|error| (3, error.to_string()))
    }))
}

/// `outbox/acknowledge`: says the outcome of a queued event was checked,
/// releasing what it holds for recovery (`outbox.acknowledge`).
pub fn acknowledge(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or((INVALID_PARAMS, "id is not an event id".to_owned()))?;
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let outbox = outbox_of(session).await?;
        outbox
            .acknowledge(&id)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error.to_string()))
    }))
}

/// `outbox/supports`: whether this connection can sign and send events
/// of `kind` (`outbox.supports`): the community takes writes from you,
/// and your key signs that kind.
pub fn supports(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let kind = params
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|kind| u16::try_from(kind).ok())
        .ok_or((INVALID_PARAMS, "kind is required".to_owned()))?;
    let identity = match &cx.global::<crate::community::AppState>().identity {
        crate::community::IdentityState::Ready(identity) => identity.clone(),
        _ => return Ok(Task::ready(Ok(json!(false)))),
    };
    // App data (NIP-78) is signed apart from what shows in conversations.
    let purpose = if kind == 30078 {
        buzz_identity::Purpose::AppData
    } else {
        buzz_identity::Purpose::Content
    };
    let signs = identity
        .template(purpose, kind, 0, Vec::new(), String::new())
        .is_ok();
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        Ok(json!(signs && session.outbox().await.is_some()))
    }))
}

fn pending(outbox: &Outbox) -> Value {
    json!(
        outbox
            .pending()
            .iter()
            .map(|op| operation(op))
            .collect::<Vec<_>>()
    )
}

async fn outbox_of(session: std::sync::Arc<buzz_relay_client::Session>) -> Result<Outbox, Failure> {
    session
        .outbox()
        .await
        .ok_or_else(|| (3, "This community takes no writes.".to_owned()))
}

/// `outbox/get`: one event's delivery while the outbox still knows it,
/// or `null`; without `id`, every queued event.
pub fn get(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let id = match params.get("id").and_then(Value::as_str) {
        None => None,
        Some(hex) => Some(
            EventId::from_hex(hex)
                .map_err(|_| (INVALID_PARAMS, "id is not an event id".to_owned()))?,
        ),
    };
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let outbox = outbox_of(session).await?;
        Ok(match id {
            None => pending(&outbox),
            Some(id) => outbox.get(&id).map_or(Value::Null, |op| operation(&op)),
        })
    }))
}

/// `outbox/watch`: sends `outbox/changed` with the queued events now and
/// whenever the queue changes.
pub fn watch(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let session = community(cx)?.read(cx).session.clone();
    let (sender, mut changes) = futures::channel::mpsc::unbounded::<Value>();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        // Before the community first connects there is no queue yet:
        // nothing is on its way, and the watch goes on once there is one.
        let mut snapshots = session.snapshot();
        let mut empty = false;
        let outbox = loop {
            if let Some(outbox) = session.outbox().await {
                break outbox;
            }
            if !empty {
                if sender.unbounded_send(json!([])).is_err() {
                    return;
                }
                empty = true;
            }
            if snapshots.changed().await.is_err() {
                return;
            }
        };
        let mut receiver = outbox.changes();
        let mut last = if empty { json!([]) } else { Value::Null };
        loop {
            let now = pending(&outbox);
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
        while let Some(queued) = changes.next().await {
            cx.update(|cx| notify(json!({ "events": queued }), cx));
        }
    });
    Ok((Watch::new((), forward), json!({})))
}
