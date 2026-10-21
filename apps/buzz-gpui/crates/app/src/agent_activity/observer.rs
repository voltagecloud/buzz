//! Runs [`Activity`] for a community: the session's live observer feed,
//! decrypted with the viewer's key off the UI thread; the session's
//! typing as working evidence; and saved pages from the local archive.
//!
//! Saved pages are read with buzz-desktop's archive (`Request::Read`),
//! the same code and database file buzz-app reads, so paging, retention
//! and the 100-record page size match it exactly; the port's
//! `buzz_relay_client::archive` only writes and configures.

use super::local::{self, LocalActivity, LocalWork};
use super::turns::TriggerMessage;
use super::{Activity, Capture, FeedStatus, Frame, HistoryRead, Page, Snapshot, current};
use crate::community::{Community, data_dir};
use crate::runtime::Runtime;
use buzz_desktop::archive::{Archive, Reply, Request};
use buzz_desktop::relay::agent::AgentEvent;
use buzz_identity::Identity;
use buzz_relay_client::session::{ActivityFeed, FeedStatus as RouteFeed};
use buzz_relay_client::{RelayOrigin, archive::community_key};
use gpui_kit::*;
use nostr::prelude::{Event, EventId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The archive database, shared by every community's reads.
#[derive(Default)]
struct SavedActivity(Archive);

impl Global for SavedActivity {}

fn archive_path() -> std::path::PathBuf {
    data_dir().join("archive").join("events.sqlite3")
}

/// Each community's activity, by community entity.
#[derive(Default)]
struct Registry(HashMap<EntityId, Entity<AgentActivity>>);

impl Global for Registry {}

/// The community's activity, once attached.
pub fn get(community: &Entity<Community>, cx: &App) -> Option<Entity<AgentActivity>> {
    cx.try_global::<Registry>()?
        .0
        .get(&community.entity_id())
        .cloned()
}

/// The community's snapshot, or an empty one.
pub fn snapshot(community: &Entity<Community>, cx: &App) -> Arc<Snapshot> {
    get(community, cx)
        .map(|activity| activity.read(cx).snapshot.clone())
        .unwrap_or_default()
}

/// Starts agent activity for a new community. The Agent Activity plugin
/// is always on here, so its lease is held for the session's life.
pub fn attach(community: &Entity<Community>, identity: Arc<Identity>, cx: &mut App) {
    let id = community.entity_id();
    let activity = cx.new(|cx| AgentActivity::new(community, identity, cx));
    cx.default_global::<Registry>().0.insert(id, activity);
    // Forget it with its community.
    cx.observe_release(community, move |_, cx| {
        cx.default_global::<Registry>().0.remove(&id);
    })
    .detach();
}

/// Every attached community with its activity entity (Mission control
/// reads them all).
pub fn all(cx: &App) -> Vec<(Entity<Community>, Entity<AgentActivity>)> {
    let Some(registry) = cx.try_global::<Registry>() else {
        return Vec::new();
    };
    registry
        .0
        .values()
        .filter_map(|activity| Some((activity.read(cx).community.upgrade()?, activity.clone())))
        .collect()
}

/// The feed state, who works where, and the working channels.
type WorkingKey = (FeedStatus, Vec<(String, Option<String>)>, Vec<String>);

/// Who works where, and in what state.
fn working_key(snapshot: &Snapshot) -> WorkingKey {
    let turns = snapshot
        .turns
        .iter()
        .filter(|turn| turn.state == super::turns::TurnState::Working)
        .map(|turn| (turn.agent.clone(), turn.channel_id.clone()))
        .chain(
            snapshot
                .typing
                .iter()
                .map(|t| (t.agent.clone(), Some(t.channel_id.clone()))),
        )
        .collect();
    (snapshot.status, turns, snapshot.working_channels.to_vec())
}

/// Who works in `channel` (or one thread of it) by observed evidence:
/// working turns with their live labels, and typing from agents that
/// send telemetry (`currentActivity` without the owned-typing part,
/// which the conversation views add from typing).
pub fn observed(
    community: &Entity<Community>,
    channel: &str,
    thread: Option<&str>,
    cx: &App,
) -> std::collections::BTreeMap<String, current::CurrentActivity> {
    let snapshot = snapshot(community, cx);
    let entries = current::typing_entries(&community.read(cx).snapshot);
    current::current_activity(Some(&snapshot), &entries, &HashSet::new(), channel, thread)
}

pub struct AgentActivity {
    community: WeakEntity<Community>,
    session: Arc<buzz_relay_client::Session>,
    identity: Arc<Identity>,
    origin: RelayOrigin,
    activity: Activity,
    pub snapshot: Arc<Snapshot>,
    /// Channels the viewer can read, as last seen.
    access: HashSet<String>,
    ticks: u64,
    /// A lookup of trigger messages is out.
    looking_up: bool,
    _local: Task<()>,
    _feed: Option<Task<()>>,
    _tick: Task<()>,
    _community: Subscription,
}

impl AgentActivity {
    fn new(community: &Entity<Community>, identity: Arc<Identity>, cx: &mut Context<Self>) -> Self {
        let (session, origin) = {
            let community = community.read(cx);
            (community.session.clone(), community.origin.clone())
        };
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    return;
                }
            }
        });
        let subscription = cx.observe(community, |this, community, cx| {
            this.community_changed(&community, cx)
        });
        let mut changes = cx.default_global::<LocalActivity>().subscribe();
        let local = cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while changes.next().await.is_some() {
                if this.update(cx, |this, cx| this.local(cx)).is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            community: community.downgrade(),
            session,
            identity,
            origin,
            activity: Activity::new(true, true),
            snapshot: Arc::default(),
            access: HashSet::new(),
            ticks: 0,
            looking_up: false,
            _local: local,
            _feed: None,
            _tick: tick,
            _community: subscription,
        };
        this.access = this.accessible(cx);
        let read = this.activity.activate(now_ms());
        this.subscribe(cx);
        this.read_history(read, cx);
        this.read_capture(cx);
        this.local(cx);
        this.publish(cx);
        this
    }

    /// Shows this community's own agents' local work, in channels the
    /// viewer can read.
    fn local(&mut self, cx: &mut Context<Self>) {
        let scope = local::scope(&self.origin.http(), self.session.viewer());
        let work: Arc<[LocalWork]> = cx
            .default_global::<LocalActivity>()
            .work()
            .iter()
            .filter(|row| row.scope == scope && self.access.contains(&row.channel_id))
            .cloned()
            .collect();
        if self.activity.set_local(work, now_ms()) {
            self.publish(cx);
        }
    }

    /// Asks the session for the messages telemetry turns name, so their
    /// requests tie to messages and threads (`messagesChanged`). Messages
    /// that arrive later are found on a later tick.
    fn look_up(&mut self, cx: &mut Context<Self>) {
        if self.looking_up {
            return;
        }
        let ids: Vec<EventId> = self
            .activity
            .unresolved()
            .iter()
            .filter_map(|id| EventId::from_hex(id).ok())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.looking_up = true;
        let found = self.session.retained_events(ids);
        cx.spawn(async move |this, cx| {
            let found: Vec<TriggerMessage> = found.await.iter().map(trigger_message).collect();
            let _ = this.update(cx, |this, cx| {
                this.looking_up = false;
                if this.activity.messages_found(&found, now_ms()) {
                    this.publish(cx);
                }
            });
        })
        .detach();
    }

    fn accessible(&self, cx: &App) -> HashSet<String> {
        self.community
            .upgrade()
            .map(|community| {
                community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .filter(|channel| !channel.cached)
                    .map(|channel| channel.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Subscribes to the live feed; frames are decrypted on the runtime.
    fn subscribe(&mut self, cx: &mut Context<Self>) {
        let mut feed = self.session.observe_activity();
        let identity = self.identity.clone();
        let (sender, mut decoded) = futures::channel::mpsc::unbounded::<Result<Frame, RouteFeed>>();
        Runtime::handle(cx).spawn(async move {
            while let Some(item) = feed.recv().await {
                let next = match item {
                    ActivityFeed::Status(status) => Err(status),
                    ActivityFeed::Frame(event) => {
                        let identity = identity.clone();
                        match tokio::task::spawn_blocking(move || decode(&identity, &event)).await {
                            Ok(Some(frame)) => Ok(frame),
                            _ => continue,
                        }
                    }
                };
                if sender.unbounded_send(next).is_err() {
                    return;
                }
            }
        });
        self._feed = Some(cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(item) = decoded.next().await {
                let updated = this.update(cx, |this, cx| {
                    let now = now_ms();
                    match item {
                        Ok(frame) => {
                            let access = this.access.clone();
                            this.activity
                                .receive(frame, now, &|channel| access.contains(channel));
                            this.look_up(cx);
                        }
                        Err(status) => {
                            tracing::debug!(?status, "agent activity feed");
                            this.activity.set_status(feed_status(status), now);
                            if status == RouteFeed::Listening {
                                this.dev_fixture();
                            }
                            this.typing(cx);
                        }
                    }
                    this.publish(cx);
                });
                if updated.is_err() {
                    return;
                }
            }
        }));
    }

    fn community_changed(&mut self, community: &Entity<Community>, cx: &mut Context<Self>) {
        let access = self.accessible(cx);
        let now = now_ms();
        if self.access.iter().any(|channel| !access.contains(channel)) {
            // Access went away: start over (`accessChanged`).
            self.access = access;
            let read = self.activity.restart(now);
            self.subscribe(cx);
            self.read_history(read, cx);
        } else if access != self.access {
            self.access = access;
            let restorable = self.access.clone();
            self.activity
                .restore_history(now, &|channel| restorable.contains(channel));
        }
        let _ = community;
        self.typing(cx);
        self.local(cx);
        self.publish(cx);
    }

    fn typing(&mut self, cx: &mut Context<Self>) {
        let Some(community) = self.community.upgrade() else {
            return;
        };
        let entries = current::typing_entries(&community.read(cx).snapshot);
        let access = self.access.clone();
        self.activity
            .typing(&entries, now_ms(), &|channel| access.contains(channel));
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks += 1;
        if self.ticks.is_multiple_of(10) {
            self.read_capture(cx);
        }
        self.activity.publish(now_ms());
        self.look_up(cx);
        self.publish(cx);
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        if *self.snapshot == *self.activity.snapshot() {
            return;
        }
        let before = working_key(&self.snapshot);
        self.snapshot = Arc::new(self.activity.snapshot().clone());
        cx.notify();
        // Conversation views follow the community; tell them when who
        // works where changed, not on every record.
        if working_key(&self.snapshot) != before {
            if let Some(community) = self.community.upgrade() {
                community.update(cx, |_, cx| cx.notify());
            }
            // Pages outside a community, such as Agents, show it too.
            cx.refresh_windows();
        }
    }

    /// "Retry live feed".
    pub fn retry(&self) {
        self.session.retry();
    }

    pub fn select_history(&mut self, agent: &str, cx: &mut Context<Self>) {
        let read = self.activity.select_history(agent, now_ms());
        self.read_history(read, cx);
        self.publish(cx);
    }

    pub fn latest_history(&mut self, cx: &mut Context<Self>) {
        let read = self.activity.latest_history(now_ms());
        self.read_history(read, cx);
        self.publish(cx);
    }

    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        let read = self.activity.load_older(now_ms());
        self.read_history(read, cx);
        self.publish(cx);
    }

    fn partition(&self) -> (String, String) {
        (self.identity.pubkey_hex(), community_key(&self.origin))
    }

    /// Reads and decrypts a saved page off the UI thread.
    fn read_history(&mut self, read: Option<HistoryRead>, cx: &mut Context<Self>) {
        let Some(read) = read else {
            return;
        };
        let (viewer, community) = self.partition();
        let identity = self.identity.clone();
        let archive = cx.default_global::<SavedActivity>().0.clone();
        let generation = read.generation;
        let task = Runtime::handle(cx).spawn_blocking(move || {
            let reply = archive.run(
                archive_path(),
                &community,
                &viewer,
                Request::Read {
                    kind: 24200,
                    agent: read.agent,
                    before: read.before,
                },
            )?;
            let Reply::Page(page) = reply else {
                return Err("Unexpected archive reply".to_string());
            };
            let decoded = identity.decode_archive_page(page)?;
            Page::parse(&decoded).ok_or_else(|| "Invalid archive page".to_string())
        });
        cx.spawn(async move |this, cx| {
            let page = task
                .await
                .unwrap_or_else(|_| Err("Archive unavailable".into()));
            let _ = this.update(cx, |this, cx| {
                let access = this.access.clone();
                this.activity
                    .history_loaded(generation, page, now_ms(), &|channel| {
                        access.contains(channel)
                    });
                this.publish(cx);
            });
        })
        .detach();
    }

    /// Whether the archive saves new activity, for the notices.
    fn read_capture(&mut self, cx: &mut Context<Self>) {
        let (viewer, community) = self.partition();
        let archive = cx.default_global::<SavedActivity>().0.clone();
        let task = Runtime::handle(cx).spawn_blocking(move || {
            match archive.run(archive_path(), &community, &viewer, Request::Settings) {
                Ok(Reply::Complete(settings)) => match settings.get("observer") {
                    Some(serde_json::Value::Bool(true)) => Capture::Saving,
                    Some(serde_json::Value::Bool(false)) => Capture::Off,
                    _ => Capture::Unknown,
                },
                _ => Capture::Error,
            }
        });
        cx.spawn(async move |this, cx| {
            let capture = task.await.unwrap_or(Capture::Unknown);
            let _ = this.update(cx, |this, cx| {
                this.activity.set_capture(capture, now_ms());
                this.publish(cx);
            });
        })
        .detach();
    }

    /// Development only: `BUZZ_DEV_ACTIVITY_FIXTURE` names a JSON file of
    /// `[{"agent": hex, "plaintext": object}]` taken as live frames whenever
    /// the feed listens (again after a renewal, with fresh times), so
    /// headless runs can render transcript and working states.
    fn dev_fixture(&mut self) {
        let Ok(path) = std::env::var("BUZZ_DEV_ACTIVITY_FIXTURE") else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(%path, "no activity fixture");
            return;
        };
        // `$NOW` and `$EARLIER` (a minute ago) keep fixture turns fresh.
        let now = chrono::Utc::now();
        let text = text.replace("$NOW", &now.to_rfc3339()).replace(
            "$EARLIER",
            &(now - chrono::Duration::seconds(60)).to_rfc3339(),
        );
        let Ok(serde_json::Value::Array(frames)) = serde_json::from_str(&text) else {
            tracing::warn!(%path, "the activity fixture is not a JSON array");
            return;
        };
        let now = now_ms();
        for (index, frame) in frames.iter().enumerate() {
            let frame = Frame {
                id: format!("{:064x}", index + 1),
                agent: frame["agent"].as_str().unwrap_or_default().to_string(),
                created_at: now / 1000,
                plaintext: frame["plaintext"].to_string(),
            };
            self.activity.receive(frame, now, &|_| true);
        }
    }
}

/// What association needs of a retained message.
fn trigger_message(event: &Event) -> TriggerMessage {
    let tags = |name: &str| {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(move |tag| tag.first().map(String::as_str) == Some(name))
            .filter_map(|tag| tag.get(1).cloned())
            .collect::<Vec<String>>()
    };
    TriggerMessage {
        id: event.id.to_hex(),
        kind: event.kind.as_u16(),
        channels: tags("h"),
        references: !tags("e").is_empty(),
        root: buzz_relay_client::unread::thread_reference(event).map(|thread| thread.root_id),
    }
}

fn feed_status(status: RouteFeed) -> FeedStatus {
    match status {
        RouteFeed::Connecting => FeedStatus::Connecting,
        RouteFeed::Listening => FeedStatus::Listening,
        RouteFeed::Interrupted => FeedStatus::Interrupted,
    }
}

/// Decrypts a live envelope for the viewer (`decode_observer`).
pub fn decode(identity: &Identity, event: &Event) -> Option<Frame> {
    let event: AgentEvent = serde_json::to_value(event)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())?;
    if event.kind != 24200 {
        return None;
    }
    let value = identity.decode_agent_frame(&event, false).ok()?;
    Frame::parse(&value)
}
