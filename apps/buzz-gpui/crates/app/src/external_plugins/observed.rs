//! What the relay's activity feed shows of agents anywhere (buzz-app's
//! `agentActivity`): their turns, typing and decrypted transcript records,
//! live and from the archive. Only an agent's owner can read its records.

use super::capabilities::{Failure, Watch, community};
use super::protocol::INVALID_PARAMS;
use crate::agent_activity::observer;
use crate::agent_activity::turns::TurnState;
use crate::agent_activity::{Capture, HistoryStatus, Snapshot};
use gpui_kit::*;
use serde_json::{Value, json};

/// The most records one answer carries, newest kept.
const MAX_RECORDS: usize = 200;

fn state(snapshot: &Snapshot, agent: Option<&str>) -> Value {
    let mine = |who: &str| agent.is_none_or(|agent| agent == who);
    let records: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| mine(&record.agent))
        .collect();
    let start = records.len().saturating_sub(MAX_RECORDS);
    json!({
        "status": snapshot.status.label(),
        "turns": snapshot.turns.iter().filter(|turn| mine(&turn.agent)).map(|turn| json!({
            "agent": turn.agent,
            "turn": turn.turn_id,
            "channel": turn.channel_id,
            "at": turn.timestamp,
            "state": match turn.state {
                TurnState::Working => "working",
                TurnState::Unknown => "unknown",
                TurnState::Ended => "ended",
            },
            "requests": turn.requests.as_ref().map(|requests| requests.iter().map(|request| json!({
                "message": request.message_id,
                "root": request.thread_root_id,
            })).collect::<Vec<_>>()),
        })).collect::<Vec<_>>(),
        "typing": snapshot.typing.iter().filter(|typing| mine(&typing.agent)).map(|typing| json!({
            "agent": typing.agent,
            "channel": typing.channel_id,
            "root": typing.thread_root,
            "since": typing.started_at,
            // The last working signal: each lives `TYPING_FRESH_MS`.
            "timestamp": typing
                .expires_at
                .saturating_sub(crate::agent_activity::turns::TYPING_FRESH_MS),
            "expiresAt": typing.expires_at,
            // Only working typing evidence is kept.
            "working": true,
        })).collect::<Vec<_>>(),
        "records": records[start..].iter().map(|record| json!({
            "id": record.id,
            "agent": record.agent,
            "kind": record.kind,
            "createdAt": record.created_at,
            "receivedAt": record.received_at,
            "historical": record.historical,
            "channels": record.channel_ids,
            "frame": serde_json::from_str::<Value>(&record.plaintext)
                .unwrap_or_else(|_| Value::String(record.plaintext.to_string())),
        })).collect::<Vec<_>>(),
        "history": match snapshot.history {
            HistoryStatus::Unavailable => "unavailable",
            HistoryStatus::Loading => "loading",
            HistoryStatus::Ready => "ready",
            HistoryStatus::Error => "error",
        },
        "hasOlder": snapshot.has_older,
        // A page other than the newest is shown, how many records it could
        // not read, and whose records it holds.
        "historyOlder": snapshot.history_older,
        "historySkipped": snapshot.history_skipped,
        "historyAgents": snapshot.history_agents,
        // Records and turn states that left the live window.
        "trimmed": snapshot.trimmed,
        // Whether the archive saves new frames.
        "capture": match snapshot.capture {
            Capture::Unknown => "unknown",
            Capture::Saving => "saving",
            Capture::Off => "off",
            Capture::Error => "error",
        },
        "workingChannels": snapshot.working_channels,
        // This app's own agents' requests in channels you can read.
        "local": snapshot.local.iter().filter(|work| mine(&work.agent)).map(|work| json!({
            "agent": work.agent,
            "channel": work.channel_id,
            "message": work.message_id,
            "root": work.thread_root_id,
            "state": format!("{:?}", work.state).to_ascii_lowercase(),
            "startedAt": work.started_at,
            "error": work.error,
            "dismissed": work.dismissed,
            "step": work.step.as_ref().map(|step| json!({ "tool": step.tool, "target": step.target })),
        })).collect::<Vec<_>>(),
    })
}

fn agent(params: &Value) -> Option<&str> {
    params.get("agent").and_then(Value::as_str)
}

/// `activity/observed`: what the feed shows now, of one `agent` or all.
pub fn get(params: &Value, cx: &App) -> Result<Value, Failure> {
    let community = community(cx)?;
    // Without a feed on this device, `status` says `unavailable`.
    let snapshot = observer::snapshot(&community, cx);
    Ok(state(&snapshot, agent(params)))
}

/// `activity/history`: reads an agent's saved records from the archive
/// (`selectHistory`), older ones with `older: true` (`loadOlder`), or the
/// newest of everyone with neither. Answers at once; `activity/watch`
/// shows them as they arrive.
pub fn history(params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let activity = observer::get(&community, cx)
        .ok_or_else(|| (3, "Agent activity is not available.".to_owned()))?;
    let older = params.get("older").and_then(Value::as_bool) == Some(true);
    let agent = agent(params).map(str::to_owned);
    activity.update(cx, |activity, cx| match (older, agent) {
        (true, _) => activity.load_older(cx),
        // That agent's newest, already chosen or not (`latestHistory`).
        (false, Some(agent)) => {
            activity.select_history(&agent, cx);
            activity.latest_history(cx);
        }
        // Everyone's newest: drop an agent chosen before, then read.
        (false, None) => {
            activity.select_history("", cx);
            activity.latest_history(cx);
        }
    });
    Ok(Value::Null)
}

/// `activity/watch`: sends `activity/changed` with what
/// `activity/observed` gives, now and whenever it changes.
pub fn watch(
    params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let community = community(cx)?;
    let activity = observer::get(&community, cx)
        .ok_or_else(|| (3, "Agent activity is not available.".to_owned()))?;
    let agent = agent(params).map(str::to_owned);
    let first = state(&activity.read(cx).snapshot, agent.as_deref());
    let last = std::cell::RefCell::new(first.clone());
    let notify = std::rc::Rc::new(notify);
    let send = notify.clone();
    cx.defer(move |cx| send(first, cx));
    let subscription = cx.observe(&activity, move |activity, cx| {
        let now = state(&activity.read(cx).snapshot, agent.as_deref());
        if now != *last.borrow() {
            *last.borrow_mut() = now.clone();
            notify(now, cx);
        }
    });
    Ok((Watch::new(subscription, Task::ready(())), json!({})))
}

fn archive_json(
    settings: &buzz_relay_client::archive::ArchiveSettings,
    path: &std::path::Path,
) -> Value {
    json!({
        "observer": settings.observer,
        "metrics": settings.metrics,
        "observerDays": settings.observer_days,
        "revision": settings.revision,
        "location": "device",
        "path": path,
        "bytes": settings.bytes,
    })
}

/// `archive/settings`: what this device saves of your agents' activity
/// in the selected community (`ArchiveHost.settings`).
pub fn archive_settings(cx: &mut App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let (settings, path) =
        crate::ui::settings::archive::settings_for(&community, cx).map_err(|error| (3, error))?;
    Ok(archive_json(&settings, &path))
}

/// `archive/configure`: saves what to keep (`ArchiveHost.configure`);
/// `revision` must be the one read, so a change made meanwhile wins.
pub fn archive_configure(params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let flag = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| (INVALID_PARAMS, format!("{key} must be true or false")))
    };
    let days = params
        .get("observerDays")
        .and_then(Value::as_u64)
        .and_then(|days| u32::try_from(days).ok())
        .ok_or((INVALID_PARAMS, "observerDays is required".to_owned()))?;
    let revision = params
        .get("revision")
        .and_then(Value::as_i64)
        .ok_or((INVALID_PARAMS, "revision is required".to_owned()))?;
    let saved = crate::ui::settings::archive::configure_for(
        &community,
        flag("observer")?,
        flag("metrics")?,
        days,
        revision,
        cx,
    )
    .map_err(|error| (3, error))?;
    let (_, path) =
        crate::ui::settings::archive::settings_for(&community, cx).map_err(|error| (3, error))?;
    Ok(archive_json(&saved, &path))
}

/// `archive/read`: one page of your agents' saved records, activity
/// (`kind` 24200) or metrics (44200), newest first, from one `agent` and
/// older than `before` (the page before's cursor) if given
/// (`ArchiveHost.read`).
pub fn archive_read(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let kind = match params.get("kind").and_then(Value::as_u64) {
        Some(kind @ (24200 | 44200)) => kind as u16,
        _ => return Err((INVALID_PARAMS, "kind is 24200 or 44200".to_owned())),
    };
    let agent = params
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let before = params.get("before").and_then(Value::as_i64);
    let read = crate::ui::settings::archive::read_for(&community, kind, agent, before, cx)
        .map_err(|error| (3, error))?;
    Ok(cx.background_spawn(async move {
        // Every agent with saved records of this kind, not only this page's.
        let (page, agents) = read.await.map_err(|error| (2, error))?;
        Ok(json!({
            "agents": agents,
            "records": page.records.iter().map(|record| json!({
                "id": record.id,
                "agent": record.agent,
                "createdAt": record.created_at,
                "receivedAt": record.received_at,
                "frame": serde_json::from_str::<Value>(&record.plaintext)
                    .unwrap_or_else(|_| Value::String(record.plaintext.clone())),
            })).collect::<Vec<_>>(),
            "before": page.before,
            "skipped": page.skipped,
            "revision": page.revision,
        }))
    }))
}

/// `archive/clear`: deletes saved activity (`kind` 24200), metrics
/// (44200), or both (`ArchiveHost.clear`, buzz-app's `clearHistory`).
pub fn archive_clear(params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let community = community(cx)?;
    let kind = match params.get("kind").and_then(Value::as_u64) {
        None => None,
        Some(kind @ (24200 | 44200)) => Some(kind as u16),
        Some(_) => return Err((INVALID_PARAMS, "kind is 24200 or 44200".to_owned())),
    };
    crate::ui::settings::archive::clear_for(&community, kind, cx).map_err(|error| (3, error))?;
    // The saved activity on show goes with it.
    if kind != Some(44200)
        && let Some(activity) = observer::get(&community, cx)
    {
        activity.update(cx, |activity, cx| activity.latest_history(cx));
    }
    Ok(Value::Null)
}
