//! One community session (`session.ts`): everything the app knows about one
//! relay for one viewer.
//!
//! An actor on the I/O runtime owns the relay connection, the reader and
//! every feature store, and handles one message at a time, so the stores
//! need no locks and a read result can never interleave with another
//! update. Reads run as separate tasks and report back as messages, fenced
//! by an access epoch so a result started before a revocation is dropped.
//!
//! The UI reads immutable snapshots from watch channels: one for the
//! session (status, channels, profiles, unread counts, presence, typing)
//! and one per open channel. Read state and unread live in [`unread`],
//! presence and typing in [`presence`], notification candidates in
//! [`incoming`].

mod account;
mod actions;
mod activity;
mod admins;
mod canvas;
mod channels;
mod control;
mod delivery;
mod disk;
mod feedback;
mod gifs;
mod inbox;
mod incoming;
mod kit;
mod live_feed;
mod me_kit;
mod members;
mod memories;
mod messaging;
mod moderation;
mod observer;
mod presence;
mod projects;
mod publisher;
mod reminders;
pub mod share;
mod sidebar;
mod statuses;
mod threads;
mod timeline;
mod unread;
mod util;
mod work_sessions;
mod workflows;

pub use actions::{DmVisibilityStatus, MessageSearch, PeoplePage, PublicChannels, SearchOutcome};
pub use feedback::{FeedbackState, PendingFeedback};
pub use inbox::{EXPIRED as INBOX_EXPIRED, InboxFeed, InboxFeedStatus, InboxState};
pub use incoming::IncomingMessage;
pub use kit::{CreationSnapshot, KitSnapshot, KitStatus};
pub use live_feed::LiveBatch;
pub use me_kit::{MeKit, MeKitSnapshot, MeKitStatus};
pub use members::{MemberAddition, MemberOperation, MemberOperationStatus};
pub use memories::{MemoryEntry, MemoryError, MemoryListing, memory_agent};
pub use observer::{ActivityFeed, FeedStatus};
pub use projects::Projects;
pub use statuses::StatusWatch;
pub use threads::ThreadView;
pub use timeline::{TimelineSnapshot, TimelineStatus};
pub use unread::{
    ChannelUnread, READING_DWELL_MS, ThreadUnread, UnreadCounts, UnreadDetail, UnreadWatch,
};
pub use work_sessions::{AgentRules, Expected, create_draft, session_description, session_title};

use crate::budget::ByteLru;
use crate::discovery::{DiscoveryState, RosterScan, ScanOptions, ScanStage, membership_hint};
use crate::emoji::{EmojiDirectory, EmojiSnapshot, catalog_filter};
use crate::event::{Verified, Verifier, is_hex64, kinds};
use crate::fold::{LocalDelivery, MessageClock, MessageRow};
use crate::http::RelayHttp;
use crate::live::{
    Live, LiveConnection, LiveOptions, LiveRoute, LiveRouteStatus, LiveSnapshot, LiveUpdate,
};
use crate::media::Media;
use crate::messages::{self, Attachment, Author, Compose, Mentions};
use crate::origin::RelayOrigin;
use crate::outbox::{HttpPublisher, Outbox, OutboxOptions, SendOptions};
use crate::presence::{
    PresenceDirectory, PresenceMode, PresencePublisher, PresenceState, Publication, PublishOutcome,
    SnapshotRead, SnapshotResult,
};
use crate::profiles::{ProfileDirectory, Profiles};
use crate::read_state::{PublishFailure, ReadMutationResult, RefreshRequest, SnapshotDescriptor};
use crate::reader::{ReadError, ReadErrorKind, ReadOptions, Reader, ReaderOptions};
use crate::socket::Filter;
use crate::store::{Partition, SqliteJournal, Store};
use crate::typing::Typing;
use crate::user_status::{StatusDirectory, Statuses};
use crate::window::Cursor;
use crate::{ChannelSummary, Events};
use buzz_identity::Identity;
use nostr::prelude::{EventId, PublicKey};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use timeline::Timeline;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use unread::{Mark, Reads};

/// Closed channels kept warm for a quick return.
const RETAINED_CLOSED: usize = 3;
/// Recently accepted events kept as visibility evidence (`session.ts`
/// `recent`).
const RECENT_EVENTS: usize = 4096;
const RECENT_BYTES: usize = 8 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting,
    Ready,
    /// Could not reach the relay or it did not identify itself. Waits for
    /// [`Session::retry`] (or, while a saved snapshot shows,
    /// [`Session::online`]).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RosterStatus {
    Loading,
    Ready,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RelayInfo {
    pub name: Option<String>,
    pub icon: Option<String>,
    /// The relay's own signing key (NIP-11 `self`).
    pub authority: String,
    pub supports_channel_creation: bool,
    /// NIP-11 `supported_extensions` lists `nip-cl`: the relay keeps
    /// channel labels and answers label lookups.
    pub channel_labels: bool,
    /// NIP-11 `read_state_snapshot`: read state can be fetched completely.
    pub read_state_snapshot: Option<SnapshotDescriptor>,
}

/// The last roster pass (`RosterRefresh`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RosterRefresh {
    #[default]
    Idle,
    Pending,
    Verified,
    /// Interrupted (by a revocation or reconnect); another pass is owed.
    Deferred,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchUpState {
    Pending,
    Verified,
    /// Not needed now (the channel is not open) or interrupted.
    Deferred,
    Error,
}

/// A channel's catch-up after the live stream was (re)established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadCatchUp {
    pub channel: String,
    pub state: CatchUpState,
    pub error: Option<String>,
}

/// Live updates as the "Live updates need attention" notice reads them
/// (`session.live`): the connection and its routes, the roster pass and
/// per-channel catch-up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveView {
    pub live: LiveSnapshot,
    pub roster: RosterRefresh,
    pub heads: Vec<HeadCatchUp>,
}

/// The reconnect wait from which a lost live connection is shown: with the
/// socket's 500 ms backoff, after three quick attempts have failed.
const LOUD_RETRY: Duration = Duration::from_secs(4);

impl LiveView {
    /// The first error worth showing for `channel` (`LiveStatus.tsx`):
    /// the connection, the roster, the channel's catch-up, then its route
    /// or a global route. Quiet while quota recovery runs by itself, and
    /// for the first quick reconnects after a drop.
    pub fn issue(&self, channel: Option<&str>) -> Option<String> {
        let route = channel.and_then(|id| {
            self.live
                .routes
                .iter()
                .find(|route| route.channel.as_deref() == Some(id))
        });
        let head = channel.and_then(|id| self.heads.iter().find(|head| head.channel == id));
        let relevant: Vec<&LiveRoute> = route
            .into_iter()
            .chain(
                self.live
                    .routes
                    .iter()
                    .filter(|route| route.channel.is_none()),
            )
            .collect();
        let recovering = |route: &LiveRoute| {
            self.live.status == LiveConnection::Connected
                && route.status == LiveRouteStatus::Pending
                && route
                    .error
                    .as_deref()
                    .and_then(crate::socket::quota_hint)
                    .is_some_and(|seconds| seconds <= 60)
        };
        let actionable: Vec<&&LiveRoute> =
            relevant.iter().filter(|route| !recovering(route)).collect();
        let roster_error = match &self.roster {
            RosterRefresh::Error(error) => Some(error.clone()),
            _ => None,
        };
        // The first few reconnects after a drop (sleep, a network change)
        // usually succeed within seconds; only a longer outage is shown.
        let quiet_retry = self.live.status == LiveConnection::Retrying
            && self.live.retry_in.is_some_and(|wait| wait < LOUD_RETRY);
        let error = (!quiet_retry)
            .then(|| self.live.error.clone())
            .flatten()
            .or(roster_error)
            .or_else(|| head.and_then(|head| head.error.clone()))
            .or_else(|| actionable.iter().find_map(|route| route.error.clone()));
        let issue = (self.live.status == LiveConnection::Retrying && !quiet_retry)
            || self.live.status == LiveConnection::Error
            || error.is_some()
            || actionable.iter().any(|route| {
                matches!(
                    route.status,
                    LiveRouteStatus::Error | LiveRouteStatus::Limited
                )
            })
            || head.is_some_and(|head| head.state == CatchUpState::Error)
            || matches!(
                self.roster,
                RosterRefresh::Error(_) | RosterRefresh::Deferred
            );
        if !issue {
            return None;
        }
        Some(match error {
            Some(error) if error.contains("rate-limited: quota exceeded") => {
                "A relay request was rate-limited; recovery needs attention.".into()
            }
            Some(error) => error,
            None if matches!(
                self.roster,
                RosterRefresh::Error(_) | RosterRefresh::Deferred
            ) =>
            {
                "Channel list needs refreshing.".into()
            }
            None => "Live updates are unavailable.".into(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub status: SessionStatus,
    /// The live socket is authenticated; otherwise views may be stale.
    pub live: bool,
    /// Showing this device's saved roster and conversations while the
    /// relay has not answered (offline resume). Nothing is writable;
    /// "Reconnect to update saved conversations."
    pub cached: bool,
    /// The saved copy is still being read from disk, before anything
    /// shows (`RelaySnapshot.restoring`).
    pub restoring: bool,
    /// Live connection, routes and catch-up. Replaced only when it changes.
    pub live_status: Arc<LiveView>,
    pub relay: Option<RelayInfo>,
    pub roster: RosterStatus,
    /// Channels the viewer is a member of, DMs included.
    pub channels: Arc<[ChannelSummary]>,
    pub profiles: Profiles,
    /// Unread rows and badges per channel. Replaced only when it changes.
    pub unread: Arc<UnreadCounts>,
    /// Observed statuses of the people passed to
    /// [`Session::watch_presence`]. Absent people are unknown: show no
    /// badge. Replaced only when it changes.
    pub presence: Arc<HashMap<String, PresenceState>>,
    /// Channel id → people typing at its top level, in the order they
    /// started. Replaced only when it changes.
    pub typing: Arc<HashMap<String, Vec<String>>>,
    /// Thread root id → people typing in that thread.
    pub thread_typing: Arc<HashMap<String, Vec<String>>>,
    /// Channel id → people typing anywhere in it, threads included.
    pub typing_anywhere: Arc<HashMap<String, Vec<String>>>,
    /// DMs the viewer hid (relay-signed 30622). The sidebar leaves them
    /// out; opening one still works.
    pub hidden_dms: Arc<[String]>,
    /// Whether [`Self::hidden_dms`] was read, is loading or failed.
    pub dm_visibility: DmVisibilityStatus,
    /// The community's custom emoji. Replaced only when it changes.
    pub emoji: Arc<EmojiSnapshot>,
    /// Sidebar preferences and local hidden DMs. Replaced only when they
    /// change.
    pub sidebar: Arc<crate::sidebar_store::SidebarState>,
    /// Current statuses of the people held by a
    /// [`Session::watch_statuses`] watch, and the viewer's once seen.
    /// Replaced only when it changes.
    pub statuses: Statuses,
}

pub struct Session {
    commands: mpsc::UnboundedSender<Message>,
    runtime: tokio::runtime::Handle,
    snapshot: watch::Receiver<Arc<SessionSnapshot>>,
    incoming: broadcast::Sender<IncomingMessage>,
    viewer: String,
    origin: RelayOrigin,
    next_thread: std::sync::atomic::AtomicU64,
    /// Bumped by each retry and cache clear (`RelaySnapshot.generation`).
    generation: std::sync::atomic::AtomicU64,
    media: Media,
    direct: account::Direct,
    reminders: Arc<reminders::Shared>,
    inbox: watch::Receiver<Arc<inbox::InboxState>>,
    /// Details saves by channel, kept through panel close and reopen.
    details: std::sync::Mutex<HashMap<String, crate::lifecycle::DetailsAttempt>>,
    /// The channel kit and channel creation with setup.
    kit: Arc<kit::KitShared>,
    /// The private recipe catalog, Me groups and placement.
    me_kit: me_kit::MeKit,
    workflows: crate::workflows::Workflows,
    /// The composer resource picker's channel homes (`projects.peekHome`).
    homes: crate::projects::Homes,
    /// Submitted shares by source conversation (`share-attempt.ts`).
    shares: std::sync::Mutex<HashMap<String, Arc<share::ShareAttempt>>>,
    task: tokio::task::JoinHandle<()>,
}

/// An open channel. Dropping it releases the view; the session keeps a few
/// recently closed channels warm.
pub struct ChannelView {
    pub snapshot: watch::Receiver<Arc<TimelineSnapshot>>,
    channel: String,
    commands: mpsc::UnboundedSender<Message>,
}

impl ChannelView {
    pub fn load_older(&self) {
        let _ = self.commands.send(Message::LoadOlder(self.channel.clone()));
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Reads the newest page again after it failed (`channels.ensure`).
    pub fn retry(&self) {
        let channel = self.channel.clone();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            if actor.relay.is_none() {
                actor.connect();
            } else if actor.timelines.contains_key(&channel) && !actor.demand_channel(&channel) {
                actor.load_head(&channel);
            }
        })));
    }

    /// Reports what the view shows while its channel is selected: the
    /// message rows fully in view (at most 128, newest last) and whether
    /// the list sits at its newest message. Rows that stay in view for
    /// [`READING_DWELL_MS`] are read, and at the bottom the channel is
    /// caught up. Report nothing in view and `at_bottom: false` while the
    /// window is hidden or unfocused. Until the first report the view is
    /// taken to be at the bottom.
    pub fn viewport(&self, visible: Vec<EventId>, at_bottom: bool) {
        let _ = self.commands.send(Message::Viewport {
            channel: self.channel.clone(),
            visible,
            at_bottom,
        });
    }
}

/// Stops a session send observer when dropped.
pub struct SendObserverHandle {
    id: u64,
    commands: mpsc::UnboundedSender<Message>,
}

impl Drop for SendObserverHandle {
    fn drop(&mut self) {
        let id = self.id;
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor
                .send_observers
                .retain(|(observer, _, _)| *observer != id);
        })));
    }
}

impl Drop for ChannelView {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::Close(self.channel.clone()));
    }
}

impl Session {
    /// `store` holds the outbox journal and other local state.
    pub fn open(
        runtime: &tokio::runtime::Handle,
        origin: RelayOrigin,
        identity: Arc<Identity>,
        store: Store,
    ) -> Self {
        let (commands, inbox) = mpsc::unbounded_channel();
        let snapshot = watch::Sender::new(Arc::new(SessionSnapshot {
            status: SessionStatus::Connecting,
            live: false,
            cached: false,
            restoring: false,
            live_status: Arc::default(),
            relay: None,
            roster: RosterStatus::Loading,
            channels: Arc::new([]),
            profiles: Profiles::default(),
            unread: Arc::default(),
            presence: Arc::default(),
            typing: Arc::default(),
            thread_typing: Arc::default(),
            typing_anywhere: Arc::default(),
            hidden_dms: Arc::new([]),
            dm_visibility: DmVisibilityStatus::Idle,
            emoji: Arc::default(),
            sidebar: Arc::default(),
            statuses: Statuses::default(),
        }));
        let (incoming, _) = broadcast::channel(64);
        let receiver = snapshot.subscribe();
        let verifier = Arc::new(Verifier::default());
        let http = RelayHttp::new(origin.clone(), identity.clone());
        let reader = Reader::spawn(
            runtime,
            Arc::new(http.clone()),
            verifier.clone(),
            ReaderOptions::default(),
        );
        let viewer = identity.pubkey_hex();
        let kit = me_kit::MeKit::new(
            commands.clone(),
            runtime.clone(),
            origin.http(),
            canvas::Canvas::new(commands.clone(), reader.clone(), identity.clone()),
        );
        let workflows = crate::workflows::Workflows::new(
            runtime.clone(),
            viewer.clone(),
            reader.clone(),
            http.clone(),
        );
        let media = Media::new(runtime.clone(), origin.clone(), identity.clone());
        let homes = crate::projects::Homes::default();
        let direct = account::Direct {
            http: http.clone(),
            reader: reader.clone(),
            identity: identity.clone(),
            status: tokio::sync::Mutex::new(()),
            emoji: tokio::sync::Mutex::new(()),
            lifecycle: tokio::sync::Mutex::new(()),
        };
        let (reminder_owner, reminders) = reminders::new();
        let (inbox_owner, inbox_view) = inbox::new();
        let actor = Actor {
            reminders: reminder_owner,
            inbox: inbox_owner,
            runtime: runtime.clone(),
            origin: origin.clone(),
            identity,
            verifier,
            http,
            reader,
            commands: commands.clone(),
            snapshot,
            live: None,
            observer: None,
            live_feed: Vec::new(),
            relay: None,
            discovery: None,
            scan: None,
            rescan: None,
            roster: RosterStatus::Loading,
            epoch: 0,
            timelines: HashMap::new(),
            closed: VecDeque::new(),
            profiles: ProfileDirectory::default(),
            selected: None,
            connecting: false,
            cached: false,
            restoring: false,
            next_generation: 0,
            channels: channels::Channels::default(),
            recent: ByteLru::new(RECENT_EVENTS, RECENT_BYTES),
            waiting: HashMap::new(),
            media: media.clone(),
            emoji_generation: 0,
            access_revision: 0,
            channels_cache: None,
            workflows_revision: None,
            known_channels: std::collections::HashSet::new(),
            send_observers: Vec::new(),
            activity: activity::Activity::default(),
            store,
            disk: disk::Disk::new(runtime),
            clock: Arc::new(MessageClock::new()),
            outbox: None,
            outbox_changes: None,
            reads: None,
            threads: HashMap::new(),
            waiting_threads: HashMap::new(),
            presence: PresenceDirectory::new(viewer.clone()),
            publisher: PresencePublisher::default(),
            typing: Typing::new(viewer.clone()),
            emoji: EmojiDirectory::new(viewer.clone()),
            emoji_reading: false,
            statuses: StatusDirectory::new(viewer.clone()),
            emoji_requested: false,
            incoming: incoming::Incoming::default(),
            incoming_sender: incoming.clone(),
            hidden_dms: Arc::new([]),
            sidebar: sidebar::SidebarPrefs::new(runtime),
            me_kit: kit.clone(),
            watches: HashMap::new(),
            view_reports: HashMap::new(),
            dm_visibility: DmVisibilityStatus::Idle,
            visibility_record: None,
            visibility_reading: false,
            members: members::Members::default(),
            roster_refresh: RosterRefresh::Idle,
            live_view: Arc::default(),
            workflows: workflows.clone(),
            homes: homes.clone(),
        };
        let task = runtime.spawn(actor.run(inbox));
        Self {
            commands,
            runtime: runtime.clone(),
            snapshot: receiver,
            incoming,
            viewer,
            origin,
            next_thread: std::sync::atomic::AtomicU64::new(1),
            generation: std::sync::atomic::AtomicU64::new(1),
            media,
            direct,
            reminders,
            inbox: inbox_view,
            details: Default::default(),
            kit: Arc::default(),
            me_kit: kit,
            workflows,
            homes,
            shares: Default::default(),
            task,
        }
    }

    pub fn snapshot(&self) -> watch::Receiver<Arc<SessionSnapshot>> {
        self.snapshot.clone()
    }

    pub fn viewer(&self) -> &str {
        &self.viewer
    }

    /// Downloads for this community's media and other image URLs.
    pub fn media(&self) -> &Media {
        &self.media
    }

    pub fn origin(&self) -> &RelayOrigin {
        &self.origin
    }

    /// Never blocks: the view starts at a loading snapshot and fills in.
    pub fn open_channel(&self, channel: &str) -> ChannelView {
        let (view, snapshot) = watch::channel(Arc::new(TimelineSnapshot::loading(channel)));
        let _ = self.commands.send(Message::Open {
            channel: channel.to_string(),
            view,
        });
        ChannelView {
            snapshot,
            channel: channel.to_string(),
            commands: self.commands.clone(),
        }
    }

    /// The channel in front of the user: its live route and catch-up go
    /// first.
    /// Opens a thread panel on `message`: the root, or with `exact` a reply
    /// to reveal in its thread.
    pub fn open_thread(&self, channel: &str, message: EventId, exact: bool) -> ThreadView {
        let id = self
            .next_thread
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (view, snapshot) =
            watch::channel(Arc::new(crate::threads::ThreadSnapshot::idle(exact)));
        let _ = self.commands.send(Message::OpenThread {
            id,
            channel: channel.to_string(),
            message,
            exact,
            view,
        });
        ThreadView {
            snapshot,
            id,
            commands: self.commands.clone(),
        }
    }

    /// Follows the unread state of rows in `channel` that a view shows; see
    /// [`UnreadWatch::set`].
    pub fn watch_unread(&self, channel: &str) -> UnreadWatch {
        let id = self
            .next_thread
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, snapshot) = watch::channel(Arc::new(UnreadDetail::default()));
        let channel = channel.to_string();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor
                .watches
                .insert(id, unread::WatchState::new(channel, sender));
            actor.publish_watches();
        })));
        UnreadWatch {
            snapshot,
            id,
            commands: self.commands.clone(),
        }
    }

    pub fn select(&self, channel: Option<String>) {
        let _ = self.commands.send(Message::Select(channel));
    }

    /// "Try again": connects when the relay has not answered (or a saved
    /// snapshot shows); otherwise retries live updates, the roster and
    /// read sync.
    pub fn retry(&self) {
        self.bump_generation();
        let _ = self.commands.send(Message::Retry);
    }

    /// Which attempt at this community's session this is: it changes with
    /// each retry and cache clear, so state keyed on it can start over
    /// (`RelaySnapshot.generation`).
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// "Retry live updates" (`session.live.retry`): a fresh reconnect
    /// budget, failed routes, the roster and owed catch-ups again. The
    /// relay's quota cooldown still applies.
    pub fn retry_live(&self) {
        let _ = self.commands.send(Message::RetryLive);
    }

    /// The network came back (the browser's `online`): a session showing
    /// a saved snapshot connects again.
    pub fn online(&self) {
        let _ = self.commands.send(Message::Online);
    }

    /// The window became visible: a saved snapshot connects again,
    /// otherwise the roster is re-read.
    pub fn visible(&self) {
        let _ = self.commands.send(Message::Visible);
    }

    /// Hover or keyboard intent on a channel row (`channels.prepare`):
    /// warms its head so opening it shows at once.
    pub fn prepare(&self, channel: &str) {
        let _ = self.commands.send(Message::Prepare(channel.to_string()));
    }

    /// "Retry messages": reads the channel's head again.
    pub fn refresh_channel(&self, channel: &str) {
        let _ = self
            .commands
            .send(Message::RefreshChannel(channel.to_string()));
    }

    /// Sees every fresh send of this session (`outbox.observeSend`), such as
    /// a mention that should wake a local agent or a message to a DM the
    /// viewer hid; the work it returns runs once that send is accepted or
    /// echoed. Observes until the returned handle drops.
    pub fn observe_send(&self, observer: crate::outbox::SendObserver) -> SendObserverHandle {
        let id = self
            .next_thread
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            let handle = actor
                .outbox
                .as_ref()
                .map(|outbox| outbox.observe_send(observer.clone()));
            actor.send_observers.push((id, observer, handle));
        })));
        SendObserverHandle {
            id,
            commands: self.commands.clone(),
        }
    }

    /// Forgets every cached head, tail and saved conversation of this
    /// community, here and on disk (Developer settings "Clear cache").
    /// Open channels load again; a session that had not connected tries
    /// again.
    pub async fn clear_cache(&self) {
        self.bump_generation();
        let (reply, done) = oneshot::channel();
        if self.commands.send(Message::ClearCache(reply)).is_ok() {
            let _ = done.await;
        }
    }

    /// Follows these people's presence (at most 256; the viewer first,
    /// then in this order). Replaces the previous set; people no longer
    /// listed lose their status. Results arrive in
    /// [`SessionSnapshot::presence`].
    pub fn watch_presence(&self, authors: Vec<String>) {
        let _ = self.commands.send(Message::WatchPresence(authors));
    }

    /// Reads the followed people's presence again soon.
    pub fn refresh_presence(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.presence.refresh(unread::now_ms());
        })));
    }

    /// Whether the machine has been idle long enough to show away. Only
    /// matters in [`PresenceMode::Auto`].
    pub fn set_idle(&self, idle: bool) {
        let _ = self.commands.send(Message::SetIdle(idle));
    }

    /// The user's own status choice.
    pub fn set_presence_mode(&self, mode: PresenceMode) {
        let _ = self.commands.send(Message::SetPresenceMode(mode));
    }

    /// Says the viewer's `agents`, which this app runs, are `online` or
    /// `offline` (owner presence, kind 20003), on the live socket:
    /// ephemeral, so nothing is queued or retried. The relay keeps only the
    /// agents the viewer owns. `Ok(false)` when this session can never send
    /// it.
    pub async fn publish_agent_presence(
        &self,
        status: &str,
        agents: Vec<String>,
    ) -> Result<bool, String> {
        let (reply, answer) = oneshot::channel();
        let status = status.to_owned();
        self.commands
            .send(Message::With(Box::new(move |actor| {
                let _ = reply.send(actor.agent_presence(&status, agents));
            })))
            .map_err(|_| "The session has closed".to_string())?;
        let prepared = answer
            .await
            .map_err(|_| "The session has closed".to_string())??;
        let Some((socket, event)) = prepared else {
            return Ok(false);
        };
        socket
            .publish(event)
            .await
            .map(|_| true)
            .map_err(|error| error.to_string())
    }

    /// Whether the app's window is on screen. While it is hidden, other
    /// people's presence is not observed and reads as unknown
    /// (`activity.visible()` in `presence.ts`).
    pub fn set_visible(&self, visible: bool) {
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.presence_visible(visible)
        })));
    }

    /// The events among `ids` this session retains (`retainedEvent`):
    /// open windows and threads, recent traffic or the outbox. Nothing is
    /// fetched.
    pub fn retained_events(
        &self,
        ids: Vec<EventId>,
    ) -> impl Future<Output = Vec<nostr::prelude::Event>> + use<> {
        let (reply, answer) = oneshot::channel();
        let queued = self.commands.send(Message::With(Box::new(move |actor| {
            let found = ids
                .iter()
                .filter_map(|id| actor.find_event(id))
                .map(|event| event.event().clone())
                .collect();
            let _ = reply.send(found);
        })));
        async move {
            match queued {
                Ok(()) => answer.await.unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        }
    }
    /// New messages from others that should notify: mentions, direct
    /// messages and replies in the viewer's conversations, still unread and
    /// not in view, each sent once. Lagging receivers lose old items.
    pub fn incoming(&self) -> broadcast::Receiver<IncomingMessage> {
        self.incoming.subscribe()
    }

    async fn write(&self, write: Write) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Message::Write { write, reply })
            .map_err(|_| "The session has closed".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("The session has closed".to_string()))
    }

    /// Sends a message, or a thread reply when `thread` names
    /// `(root, parent)`, with uploaded `attachments`. Resolves once the
    /// intent is safely queued; the timeline shows its delivery.
    ///
    /// The intent is handed to the session when this is called, not when
    /// the future is first polled: a caller on the UI thread would
    /// otherwise wait for its executor's next turn, often a frame, before
    /// the message shows.
    pub fn send(
        &self,
        channel: &str,
        content: &str,
        mentions: Mentions,
        attachments: Vec<Attachment>,
        thread: Option<(EventId, EventId)>,
    ) -> impl Future<Output = Result<(), String>> + use<> {
        let (reply, answer) = oneshot::channel();
        let queued = self.commands.send(Message::Write {
            write: Write::Send {
                channel: channel.to_string(),
                content: content.to_string(),
                mentions,
                attachments,
                thread,
            },
            reply,
        });
        async move {
            queued.map_err(|_| "The session has closed".to_string())?;
            answer
                .await
                .unwrap_or_else(|_| Err("The session has closed".to_string()))
        }
    }

    /// Reads the custom emoji catalog again, e.g. after an error.
    pub fn refresh_emoji(&self) {
        let _ = self.commands.send(Message::RefreshEmoji);
    }

    /// Loads the custom emoji catalog the first time something needs it
    /// (`emoji.ensure`): the picker, a shortcode search or a draft that
    /// names a code. Nothing reads it before.
    pub fn ensure_emoji(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.ensure_emoji();
        })));
    }

    pub async fn edit(&self, channel: &str, id: EventId, content: &str) -> Result<(), String> {
        self.write(Write::Edit {
            channel: channel.to_string(),
            id,
            content: content.to_string(),
        })
        .await
    }

    pub async fn react(&self, channel: &str, id: EventId, emoji: &str) -> Result<(), String> {
        self.write(Write::React {
            channel: channel.to_string(),
            id,
            content: emoji.to_string(),
            emoji: None,
        })
        .await
    }

    /// Deletes our own message or reaction, or a message of an agent the
    /// viewer owns: ownership is the NIP-OA attestation on the agent's
    /// newest signed profile, read fresh.
    pub async fn remove(&self, channel: &str, id: EventId) -> Result<(), String> {
        let agent = self.removal_agent(channel, id).await?;
        self.write(Write::Remove {
            channel: channel.to_string(),
            id,
            agent,
        })
        .await
    }

    /// Reports a loaded message (NIP-56 kind 1984). Reports are never
    /// echoed, so they go straight to the relay rather than the outbox;
    /// resolves on the relay's acceptance.
    pub async fn report(
        &self,
        channel: &str,
        id: EventId,
        kind: messages::ReportType,
        note: &str,
    ) -> Result<(), String> {
        self.report_message(channel, id, kind, note).await
    }

    /// Copies the viewer's own thread reply into the channel as a new
    /// top-level message ("Also send to channel"), with a provenance tag
    /// naming the thread. `root` is the thread's folded root, for the
    /// excerpt.
    pub async fn send_to_channel(
        &self,
        row: Arc<MessageRow>,
        root: Option<Arc<MessageRow>>,
    ) -> Result<(), String> {
        self.write(Write::SendToChannel { row, root }).await
    }

    /// Republishes the exact signed bytes of a failed or unknown send.
    pub async fn retry_send(&self, id: EventId) -> Result<(), String> {
        self.write(Write::Retry(id)).await
    }

    /// Discards a failed send.
    pub async fn dismiss(&self, id: EventId) -> Result<(), String> {
        self.write(Write::Dismiss(id)).await
    }

    async fn mark(&self, mark: Mark) -> Result<(), String> {
        self.marked(mark).await.map(drop)
    }

    async fn marked(&self, mark: Mark) -> Result<Vec<(String, ReadMutationResult)>, String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Message::Mark { mark, reply })
            .map_err(|_| "The session has closed".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("The session has closed".to_string()))
    }

    /// Reads the whole channel, through the newest message seen or now,
    /// whichever is later, and clears its manual unread marks. Saved at
    /// once; synced to the viewer's other devices shortly after.
    pub async fn mark_channel_read(&self, channel: &str) -> Result<(), String> {
        self.mark(Mark::Channel(channel.to_string())).await
    }

    /// Marks every channel with anything unread as read, with one cut,
    /// answering each channel it marked and how that mark syncs.
    pub async fn mark_all_read(&self) -> Result<Vec<(String, ReadMutationResult)>, String> {
        self.marked(Mark::All).await
    }

    /// Reads one loaded message and its loaded replies.
    pub async fn mark_message_read(&self, channel: &str, id: EventId) -> Result<(), String> {
        self.mark(Mark::MessageRead(channel.to_string(), id)).await
    }

    /// Clears a device-local "mark unread" (`clearUnreadLocal`).
    pub async fn clear_unread_local(&self, channel: &str) -> Result<(), String> {
        self.mark(Mark::ClearChannelUnread(channel.to_string()))
            .await
    }

    /// "Refresh": re-reads read markers and the unread sample in the
    /// foreground (`unread.refresh`).
    pub fn refresh_unread(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.refresh_unread();
        })));
    }

    /// "Retry read sync": re-reads markers, then publishes what waits
    /// (`unread.retrySync`).
    pub fn retry_read_sync(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.retry_read_sync();
        })));
    }

    /// The channel's unread thread activity (`activity`): unread replies in
    /// followed threads, mentions and broadcasts, grouped by root, newest
    /// first, with edited previews. For the activity popover.
    pub async fn thread_activity(
        &self,
        channel: &str,
    ) -> Option<Arc<crate::unread::ThreadActivitySnapshot>> {
        let (reply, answer) = oneshot::channel();
        let channel = channel.to_string();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            let activity = actor
                .reads
                .as_mut()
                .map(|reads| reads.unread.activity(&channel, &reads.sync));
            let _ = reply.send(activity);
        })));
        answer.await.ok().flatten()
    }

    /// Shows a loaded message (and its loaded replies) as unread for the
    /// rest of this channel visit, and the channel as unread on this
    /// device until it is next opened. Never synced.
    pub async fn mark_message_unread(&self, channel: &str, id: EventId) -> Result<(), String> {
        self.mark(Mark::MessageUnread(channel.to_string(), id))
            .await
    }

    /// Shows the channel as unread on this device until it is next read.
    /// Never synced.
    pub async fn mark_channel_unread(&self, channel: &str) -> Result<(), String> {
        self.mark(Mark::ChannelUnread(channel.to_string())).await
    }

    /// Marks a channel, thread or message unread on this device, or with
    /// `clear` drops that mark (`markUnreadLocal`, `clearUnreadLocal`).
    /// Never synced.
    pub async fn mark_local(
        &self,
        target: crate::read_state::ReadTarget,
        clear: bool,
    ) -> Result<(), String> {
        self.mark(Mark::Local { target, clear }).await
    }

    /// Reads the channel, or with `root` that thread, through `message`.
    pub async fn mark_through(
        &self,
        channel: &str,
        root: Option<EventId>,
        message: EventId,
    ) -> Result<(), String> {
        self.mark(Mark::Through {
            channel: channel.to_string(),
            root,
            message,
        })
        .await
    }
}

enum Write {
    Send {
        channel: String,
        content: String,
        mentions: Mentions,
        attachments: Vec<Attachment>,
        thread: Option<(EventId, EventId)>,
    },
    Edit {
        channel: String,
        id: EventId,
        content: String,
    },
    React {
        channel: String,
        id: EventId,
        content: String,
        /// A reaction pill's own emoji tag, used over the catalog's.
        emoji: Option<messages::CustomEmoji>,
    },
    Remove {
        channel: String,
        id: EventId,
        /// The agent whose message the viewer removes as its owner.
        agent: Option<String>,
    },
    SendToChannel {
        row: Arc<MessageRow>,
        root: Option<Arc<MessageRow>>,
    },
    Retry(EventId),
    Dismiss(EventId),
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Message {
    OpenThread {
        id: u64,
        channel: String,
        message: EventId,
        exact: bool,
        view: watch::Sender<Arc<crate::threads::ThreadSnapshot>>,
    },
    CloseThread(u64),
    ThreadMore(u64),
    ThreadRefresh(u64),
    ThreadRead {
        id: u64,
        token: u64,
        channel: String,
        result: Result<Events, ReadError>,
    },
    Open {
        channel: String,
        view: watch::Sender<Arc<TimelineSnapshot>>,
    },
    Close(String),
    LoadOlder(String),
    Select(Option<String>),
    Prepare(String),
    RefreshChannel(String),
    Retry,
    RetryLive,
    Online,
    Visible,
    ClearCache(oneshot::Sender<()>),
    Write {
        write: Write,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The device cache, read and verified off the actor.
    Restored(Option<Box<channels::Restored>>),
    StartupSaved,
    Connected(Result<RelayInfo, ConnectError>),
    ScanRead {
        epoch: u64,
        result: Result<Events, ReadError>,
    },
    Page {
        channel: String,
        sent: Option<Cursor>,
        purpose: channels::PageFor,
        epoch: u64,
        access: u64,
        result: Result<channels::PageRead, ReadError>,
    },
    Profiles {
        ids: Vec<String>,
        result: Result<Events, ReadError>,
    },
    Mark {
        mark: Mark,
        reply: oneshot::Sender<Result<Vec<(String, ReadMutationResult)>, String>>,
    },
    Viewport {
        channel: String,
        visible: Vec<EventId>,
        at_bottom: bool,
    },
    ReadRefresh {
        request: RefreshRequest,
        result: Result<Vec<Verified>, String>,
    },
    ReadPublished(Result<(), PublishFailure>),
    ReadBack(Result<Vec<Verified>, String>),
    Repair(Result<Events, ReadError>),
    Homes {
        request: crate::unread::HomesRequest,
        result: Result<Events, ReadError>,
    },
    Lookup(Result<Events, ReadError>),
    WatchPresence(Vec<String>),
    SetIdle(bool),
    SetPresenceMode(PresenceMode),
    PresenceRead {
        read: SnapshotRead,
        result: SnapshotResult,
    },
    PresencePublished {
        publication: Publication,
        outcome: PublishOutcome,
    },
    /// Runs on the actor between messages: how session operations read and
    /// change its state.
    With(Box<dyn FnOnce(&mut Actor) + Send>),
    RefreshEmoji,
    EmojiRead {
        generation: u64,
        result: Result<Events, ReadError>,
    },
    /// Stop for good; reply once nothing more will be written.
    Dispose(oneshot::Sender<()>),
}

/// Why connecting failed.
#[derive(Debug)]
struct ConnectError {
    message: String,
    /// The relay refused this viewer (401/403): nothing saved may show.
    denied: bool,
}

struct Actor {
    runtime: tokio::runtime::Handle,
    origin: RelayOrigin,
    identity: Arc<Identity>,
    verifier: Arc<Verifier>,
    http: RelayHttp,
    reader: Reader,
    commands: mpsc::UnboundedSender<Message>,
    snapshot: watch::Sender<Arc<SessionSnapshot>>,
    live: Option<Live>,
    /// The agent activity capture, if the archive wants one.
    observer: Option<observer::Observer>,
    /// Receivers of live events (`Session::live_events`).
    live_feed: Vec<mpsc::UnboundedSender<LiveBatch>>,
    relay: Option<RelayInfo>,
    discovery: Option<DiscoveryState>,
    scan: Option<RosterScan>,
    /// Another scan was asked for while one ran.
    rescan: Option<ScanOptions>,
    roster: RosterStatus,
    /// Bumped on revocation and reconnect; fences stale read results.
    epoch: u64,
    timelines: HashMap<String, Timeline>,
    /// Closed channels kept warm, most recent last.
    closed: VecDeque<String>,
    /// Views opened before the relay identified itself, by channel.
    waiting: HashMap<String, Vec<watch::Sender<Arc<TimelineSnapshot>>>>,
    profiles: ProfileDirectory,
    selected: Option<String>,
    /// NIP-11 is being read.
    connecting: bool,
    /// Showing the saved snapshot without a relay (offline resume).
    cached: bool,
    /// The saved snapshot is still being read.
    restoring: bool,
    /// Window generations come from one counter, so a window recreated
    /// after eviction never accepts a read meant for its predecessor.
    next_generation: u64,
    channels: channels::Channels,
    /// Recently accepted events, as visibility evidence.
    recent: ByteLru<Verified>,
    media: Media,
    store: Store,
    /// The actor's store writes, in order.
    disk: disk::Disk,
    /// Shared by the outbox and every timeline, so sends sort after what
    /// is shown.
    clock: Arc<MessageClock>,
    outbox: Option<Outbox>,
    outbox_changes: Option<watch::Receiver<u64>>,
    /// Read state and unread, from NIP-11 on.
    reads: Option<Reads>,
    threads: HashMap<u64, threads::OpenThread>,
    /// Thread views opened before the relay identified itself.
    waiting_threads: HashMap<u64, threads::WaitingThread>,
    presence: PresenceDirectory,
    publisher: PresencePublisher,
    typing: Typing,
    emoji: EmojiDirectory,
    /// The catalog read is in flight.
    emoji_reading: bool,
    statuses: StatusDirectory,
    /// Something asked for the catalog; until then it is not read.
    emoji_requested: bool,
    /// Bumped when the catalog is cleared or re-read; fences old reads.
    emoji_generation: u64,
    incoming: incoming::Incoming,
    incoming_sender: broadcast::Sender<IncomingMessage>,
    hidden_dms: Arc<[String]>,
    sidebar: sidebar::SidebarPrefs,
    /// The recipe catalog, shared with [`Session::kit`].
    me_kit: me_kit::MeKit,
    /// Open [`UnreadWatch`]es by id.
    watches: HashMap<u64, unread::WatchState>,
    /// What each open channel view last reported it shows.
    view_reports: HashMap<String, (Vec<EventId>, bool)>,
    dm_visibility: DmVisibilityStatus,
    /// The hidden DM list shown, for newer-wins.
    visibility_record: Option<crate::lifecycle::VisibilityRecord>,
    visibility_reading: bool,
    /// Member additions and administration in flight or unresolved.
    members: members::Members,
    roster_refresh: RosterRefresh,
    live_view: Arc<LiveView>,
    /// The discovery access revision last acted on.
    access_revision: u64,
    /// The channels last published and the discovery revision they came
    /// from: rebuilding and sorting every summary is the cost of a publish.
    channels_cache: Option<(u64, Arc<[ChannelSummary]>)>,
    /// The discovery revision workflow access was last set from, `None`
    /// inside when there was no discovery.
    workflows_revision: Option<Option<u64>>,
    /// Channels readable at the last access check.
    known_channels: std::collections::HashSet<String>,
    /// Channel activity for the "recent" sidebar order.
    activity: activity::Activity,
    /// The viewer's reminders (kind 30300).
    reminders: reminders::Owner,
    /// The inbox and its history read.
    inbox: inbox::Owner,
    workflows: crate::workflows::Workflows,
    /// Cleared when the stream drops and on Clear cache.
    homes: crate::projects::Homes,
    /// Send observers, attached to the outbox once it exists.
    send_observers: Vec<(
        u64,
        crate::outbox::SendObserver,
        Option<crate::outbox::SendObservation>,
    )>,
}

impl Actor {
    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Message>) {
        self.sidebar_restore();
        self.begin_restore();
        loop {
            let deadline = [
                self.reads_deadline(),
                self.presence_deadline(),
                self.statuses_deadline(),
            ]
            .into_iter()
            .flatten()
            .min();
            let outbox_changed = async {
                match &mut self.outbox_changes {
                    Some(changes) => changes.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            let live_update = async {
                match &mut self.live {
                    Some(live) => Some(live.next().await),
                    None => std::future::pending().await,
                }
            };
            let timer = async move {
                match deadline {
                    Some(at) => {
                        let wait = at.saturating_sub(unread::now_ms());
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                    }
                    None => std::future::pending().await,
                }
            };
            let message = tokio::select! {
                message = inbox.recv() => message,
                () = timer => {
                    self.reads_tick();
                    self.presence_tick();
                    self.statuses_tick();
                    continue;
                }
                Some(update) = live_update => {
                    self.live_update(update);
                    continue;
                }
                changed = outbox_changed => {
                    if changed {
                        self.local_changed();
                    } else {
                        self.outbox_changes = None;
                    }
                    continue;
                }
            };
            let Some(message) = message else {
                return;
            };
            if let Message::Dispose(reply) = message {
                if let Some(outbox) = &self.outbox {
                    outbox.dispose();
                }
                // Nothing may write the store once disposed.
                let _ = self.disk.flushed().await;
                let _ = reply.send(());
                return;
            }
            self.handle(message);
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::OpenThread {
                id,
                channel,
                message,
                exact,
                view,
            } => self.open_thread(id, channel, message, exact, view),
            Message::CloseThread(id) => {
                self.threads.remove(&id);
                self.waiting_threads.remove(&id);
                self.thread_closed(id);
            }
            Message::ThreadMore(id) => self.thread_more(id, false),
            Message::ThreadRefresh(id) => self.thread_more(id, true),
            Message::ThreadRead {
                id,
                token,
                channel,
                result,
            } => self.thread_result(id, token, &channel, result),
            Message::Open { channel, view } => self.open(&channel, view),
            Message::Close(channel) => self.close(&channel),
            Message::LoadOlder(channel) => self.load_older(&channel),
            Message::Select(channel) => {
                self.selected = channel;
                self.update_interests();
                self.update_reading();
            }
            Message::Prepare(channel) => self.prepare(&channel),
            Message::RefreshChannel(channel) => {
                if self.timelines.contains_key(&channel) && !self.demand_channel(&channel) {
                    self.load_head(&channel);
                }
            }
            Message::Retry => {
                if self.relay.is_none() {
                    self.connect();
                } else {
                    self.retry_live();
                    self.retry_reads();
                }
                // Watchers hear of the new generation even when nothing
                // else changed, so state kept per attempt starts over
                // (`reset()` of a ready session).
                self.snapshot.send_modify(|_| {});
            }
            Message::RetryLive => self.retry_live(),
            Message::Online => {
                if self.cached {
                    self.connect();
                }
            }
            Message::Visible => {
                if self.cached {
                    self.connect();
                } else {
                    self.start_scan(ScanOptions {
                        force_metadata: true,
                        ..Default::default()
                    });
                }
            }
            Message::ClearCache(reply) => {
                self.homes.clear();
                let cleared = self.clear_cache();
                let reconnect = self.relay.is_none();
                let commands = self.commands.clone();
                self.runtime.spawn(async move {
                    let _ = cleared.await;
                    if reconnect {
                        let _ = commands.send(Message::Online);
                    }
                    let _ = reply.send(());
                });
            }
            Message::Restored(restored) => self.restored(restored.map(|restored| *restored)),
            Message::StartupSaved => self.startup_saved(),
            Message::Connected(result) => self.connected(result),
            Message::Write {
                write: Write::Dismiss(id),
                reply,
            } => match &self.outbox {
                Some(outbox) => {
                    let dismissed = outbox.dismiss(&id);
                    self.runtime.spawn(async move {
                        let _ = reply.send(dismissed.await.map_err(|error| error.to_string()));
                    });
                }
                None => {
                    let _ = reply.send(Err("Not connected to the relay yet".into()));
                }
            },
            Message::Write { write, reply } => {
                let sending = match &write {
                    Write::Send { channel, .. } => self.sidebar_sending(channel),
                    _ => None,
                };
                let result = self.write(write);
                if result.is_ok() {
                    self.sidebar_sent(sending);
                }
                let _ = reply.send(result);
            }
            Message::ScanRead { epoch, result } => {
                if epoch == self.epoch {
                    self.scan_read(result);
                } else {
                    // A revocation interrupted the pass; another is owed.
                    self.scan = None;
                    self.roster_refresh = RosterRefresh::Deferred;
                    self.start_scan(ScanOptions::default());
                }
            }
            Message::Page {
                channel,
                sent,
                purpose,
                epoch,
                access,
                result,
            } => self.page_read(channel, sent, purpose, (epoch, access), result),
            Message::Profiles { ids, result } => {
                self.profiles.finished(&ids, result.is_ok());
                if let Ok(events) = result {
                    let wanted: std::collections::HashSet<&String> = ids.iter().collect();
                    let events: Vec<Verified> = events
                        .iter()
                        .filter(|event| wanted.contains(&event.pubkey_hex()))
                        .cloned()
                        .collect();
                    if self.profiles.accept(&events) {
                        if self.channels.discovery_observed {
                            self.save_startup();
                        }
                        self.publish();
                    }
                }
            }
            Message::Mark { mark, reply } => {
                let _ = reply.send(self.mark(mark));
            }
            Message::Viewport {
                channel,
                visible,
                at_bottom,
            } => self.viewport(&channel, visible, at_bottom),
            Message::ReadRefresh { request, result } => self.read_refreshed(request, result),
            Message::ReadPublished(outcome) => self.read_published(outcome),
            Message::ReadBack(result) => self.read_back(result),
            Message::Repair(result) => self.repaired(result),
            Message::Homes { request, result } => self.homes(request, result),
            Message::Lookup(result) => self.looked_up(result),
            Message::WatchPresence(authors) => self.watch_presence(authors),
            Message::SetIdle(idle) => {
                self.publisher.set_idle(idle, unread::now_ms());
            }
            Message::SetPresenceMode(mode) => {
                self.publisher.set_mode(mode, unread::now_ms());
            }
            Message::PresenceRead { read, result } => self.presence_read(read, result),
            Message::PresencePublished {
                publication,
                outcome,
            } => self.presence_published(publication, outcome),
            Message::With(run) => run(self),
            Message::RefreshEmoji => {
                self.emoji_requested = true;
                self.read_emoji();
            }
            Message::EmojiRead { generation, result } => {
                if generation != self.emoji_generation {
                    return;
                }
                self.emoji_reading = false;
                let result = result.as_deref().map_err(|error| error.message.clone());
                if self.emoji.finished(result) {
                    self.publish();
                }
            }
            Message::Dispose(_) => unreachable!("handled by the run loop"),
        }
    }

    // ----- connection -----

    /// Reads NIP-11 to learn the relay's signing key (`service.retry`),
    /// within 8 s. A failure waits for the user's retry, or for the network
    /// to come back while a saved snapshot shows.
    fn connect(&mut self) {
        if self.connecting || self.relay.is_some() {
            return;
        }
        self.connecting = true;
        self.set_status(SessionStatus::Connecting);
        let http = self.http.clone();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let info = tokio::time::timeout(Duration::from_secs(8), http.information()).await;
            let result = match info {
                Ok(Ok(info)) => relay_info(&info).map_err(|message| ConnectError {
                    message,
                    denied: false,
                }),
                Ok(Err(error)) => Err(ConnectError {
                    denied: matches!(
                        error,
                        crate::http::HttpError::Status {
                            status: 401 | 403,
                            ..
                        }
                    ),
                    message: error.to_string(),
                }),
                Err(_) => Err(ConnectError {
                    message: "Relay connection timed out. Try again.".to_string(),
                    denied: false,
                }),
            };
            let _ = commands.send(Message::Connected(result));
        });
    }

    fn connected(&mut self, result: Result<RelayInfo, ConnectError>) {
        self.connecting = false;
        let info = match result {
            Ok(info) => info,
            Err(error) => {
                if error.denied {
                    // Refused: nothing saved for this viewer may show.
                    drop(self.clear_cache());
                    self.discovery = None;
                    self.cached = false;
                    self.channels.cached_author = None;
                }
                self.set_status(SessionStatus::Failed(error.message));
                self.publish();
                return;
            }
        };
        self.confirm_cached_author(&info.authority);
        self.cached = false;
        if self.discovery.is_none() {
            self.discovery = Some(DiscoveryState::new(
                self.identity.pubkey_hex(),
                info.authority.clone(),
            ));
        }
        self.access_revision = self
            .discovery
            .as_ref()
            .map_or(0, DiscoveryState::access_revision);
        self.remember_known();
        self.relay = Some(info);
        self.live = Some(Live::connect(
            &self.runtime,
            self.origin.clone(),
            self.identity.clone(),
            self.verifier.clone(),
            LiveOptions::default(),
        ));
        self.observe_live();
        let journal = SqliteJournal::new(
            self.store.clone(),
            Partition::new(&self.origin, &self.identity.pubkey_hex()),
        );
        let publisher = publisher::SessionPublisher {
            http: HttpPublisher::new(self.http.clone(), self.reader.clone()),
            reader: self.reader.clone(),
            commands: self.commands.clone(),
            relay: self
                .relay
                .as_ref()
                .map(|relay| relay.authority.clone())
                .unwrap_or_default(),
            viewer: self.identity.pubkey_hex(),
            workflows: self.workflows.clone(),
        };
        let outbox = Outbox::new(
            self.identity.clone(),
            Arc::new(publisher),
            Arc::new(journal),
            OutboxOptions {
                message_clock: self.clock.clone(),
                on_receipt: Some(self.workflows.receipt_listener()),
                ..Default::default()
            },
        );
        self.workflows.attach(outbox.clone());
        self.outbox_changes = Some(outbox.changes());
        for (_, observer, handle) in &mut self.send_observers {
            *handle = Some(outbox.observe_send(observer.clone()));
        }
        self.outbox = Some(outbox);
        self.open_reads();
        self.set_status(SessionStatus::Ready);
        self.sidebar_connected();
        self.start_scan(ScanOptions::default());
        self.read_emoji();
        self.statuses_schedule();
        // Views opened before the relay answered start now.
        for (channel, views) in std::mem::take(&mut self.waiting) {
            for view in views {
                self.open(&channel, view);
            }
        }
        self.open_waiting_threads();
        // Saved heads shown while offline revalidate.
        for channel in self.open_channels() {
            self.ensure(&channel);
        }
        self.update_interests();
        self.publish();
    }

    /// "Retry live updates" (`live.retry`).
    fn retry_live(&mut self) {
        if matches!(
            self.roster_refresh,
            RosterRefresh::Error(_) | RosterRefresh::Deferred
        ) {
            self.start_scan(ScanOptions {
                strong: true,
                ..Default::default()
            });
        }
        for channel in self.demanded_open() {
            self.demand_channel(&channel);
        }
        if let Some(live) = &mut self.live {
            live.retry();
        }
        // Suspended previews are looked up again (`revalidatePreviews`).
        let suspended = self
            .discovery
            .as_ref()
            .map(DiscoveryState::suspended_channels)
            .unwrap_or_default();
        self.revalidate_previews(&suspended);
    }

    /// Looks `suspended` public previews up again, 128 at a time; one
    /// that resolves resumes and its recheck notice goes.
    fn revalidate_previews(&self, suspended: &[String]) {
        for chunk in suspended.chunks(128) {
            let ops = actions::Ops {
                commands: self.commands.clone(),
            };
            let ids = chunk.to_vec();
            tokio::spawn(async move {
                if let Ok(ctx) = ops.context().await {
                    let _ = ops.resolve(&ctx, ids).await;
                }
            });
        }
    }

    /// Open windows, the selected one first.
    fn demanded_open(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .selected
            .iter()
            .filter(|id| self.timelines.contains_key(*id))
            .cloned()
            .collect();
        ids.extend(
            self.timelines
                .keys()
                .filter(|id| Some(*id) != self.selected.as_ref())
                .cloned(),
        );
        ids
    }

    fn set_status(&mut self, status: SessionStatus) {
        self.snapshot.send_if_modified(|current| {
            if current.status == status {
                return false;
            }
            *current = Arc::new(SessionSnapshot {
                status,
                ..(**current).clone()
            });
            true
        });
    }

    fn relay_author(&self) -> Option<PublicKey> {
        self.relay
            .as_ref()
            .and_then(|relay| PublicKey::from_hex(&relay.authority).ok())
    }

    // ----- roster -----

    fn start_scan(&mut self, options: ScanOptions) {
        if self.relay.is_none() {
            return;
        }
        let Some(discovery) = &self.discovery else {
            return;
        };
        if self.scan.is_some() {
            let pending = self.rescan.unwrap_or_default();
            self.rescan = Some(ScanOptions {
                strong: pending.strong || options.strong,
                force_metadata: pending.force_metadata || options.force_metadata,
            });
            return;
        }
        self.scan = Some(RosterScan::new(discovery, options));
        if !matches!(self.roster_refresh, RosterRefresh::Pending) {
            self.roster_refresh = RosterRefresh::Pending;
            self.publish_live();
        }
        self.scan_next();
    }

    fn scan_next(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        let Some(read) = scan.next_read() else {
            self.scan_done();
            return;
        };
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        let epoch = self.epoch;
        self.runtime.spawn(async move {
            let result = reader
                .read(
                    vec![read.filter],
                    ReadOptions {
                        fresh: read.fresh,
                        ..Default::default()
                    },
                )
                .await;
            let _ = commands.send(Message::ScanRead { epoch, result });
        });
    }

    fn scan_read(&mut self, result: Result<Events, ReadError>) {
        let (Some(scan), Some(discovery)) = (&mut self.scan, &mut self.discovery) else {
            return;
        };
        let mut denied_all = false;
        match result {
            Ok(events) => {
                self.channels.discovery_observed = true;
                if let Err(error) = scan.accept(discovery, &events) {
                    self.roster = RosterStatus::Error(error.to_string());
                    self.roster_refresh = RosterRefresh::Error(error.to_string());
                }
            }
            Err(error) => {
                denied_all = scan.fail(discovery, error.kind == ReadErrorKind::Denied);
                self.roster_refresh = if error.kind == ReadErrorKind::Cancelled {
                    RosterRefresh::Deferred
                } else {
                    RosterRefresh::Error(error.message.clone())
                };
                self.roster = RosterStatus::Error(error.message);
            }
        }
        let done = scan.stage() == ScanStage::Done;
        if denied_all {
            // The roster itself was refused: nothing saved stays either.
            self.retain_saved_heads(std::collections::HashSet::new());
            self.check_access();
        }
        if done {
            self.scan_done();
        } else {
            self.scan_next();
        }
    }

    fn scan_done(&mut self) {
        let scan = self.scan.take();
        let certified = scan.as_ref().is_some_and(RosterScan::certified);
        if certified || !matches!(self.roster, RosterStatus::Error(_)) {
            self.roster = RosterStatus::Ready;
        }
        if !matches!(
            self.roster_refresh,
            RosterRefresh::Error(_) | RosterRefresh::Deferred
        ) {
            self.roster_refresh = RosterRefresh::Verified;
        }
        self.check_access();
        if certified && let Some(discovery) = &self.discovery {
            // Only a complete roster proves which saved heads are stale.
            let joined = discovery.channels().into_iter().map(|c| c.id).collect();
            self.retain_saved_heads(joined);
        }
        self.update_interests();
        self.reads_channels(true);
        self.ensure_profiles(self.channel_people());
        self.refresh_visibility();
        self.sidebar_probe();
        self.refresh_session_windows();
        self.refresh_activity();
        // Saved membership the pass confirmed makes saved heads readable.
        for channel in self.open_channels() {
            self.ensure(&channel);
        }
        self.save_startup();
        self.publish_live();
        self.publish();
        if let Some(options) = self.rescan.take() {
            self.start_scan(options);
        }
    }

    /// Acts on a change of channel access (`revokeAccess`): what can no
    /// longer be read goes, and everything retained is filtered again.
    fn check_access(&mut self) {
        // A suspended preview's recheck notice follows discovery.
        self.publish_live();
        let Some(discovery) = &self.discovery else {
            return;
        };
        let revision = discovery.access_revision();
        if revision == self.access_revision {
            return;
        }
        self.access_revision = revision;
        // Channels something was kept for that are no longer readable.
        let lost: Vec<String> = self
            .known_channels
            .iter()
            .chain(self.timelines.keys())
            .filter(|id| !discovery.can_access(id))
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        self.purge_denied();
        for id in &lost {
            self.bump_access(id);
            self.channels_forget(id);
        }
        self.revoke_access(&lost);
        self.remember_known();
    }

    /// The channels this session keeps anything for, to tell which a
    /// revocation took away.
    fn remember_known(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        self.known_channels = discovery
            .channels()
            .into_iter()
            .map(|channel| channel.id)
            .chain(self.timelines.keys().cloned())
            .filter(|id| discovery.can_access(id))
            .collect();
    }

    /// Drops timelines of channels the viewer can no longer read. Returns
    /// whether any went.
    fn purge_denied(&mut self) -> bool {
        let Some(discovery) = &self.discovery else {
            return false;
        };
        let denied: Vec<String> = self
            .timelines
            .keys()
            .filter(|id| !discovery.can_access(id))
            .cloned()
            .collect();
        self.threads_purge();
        if denied.is_empty() {
            return false;
        }
        self.epoch += 1;
        for id in denied {
            self.bump_access(&id);
            self.channels_forget(&id);
            if let Some(mut timeline) = self.timelines.remove(&id) {
                self.next_generation += 1;
                timeline.deny("Conversation is unavailable".into(), self.next_generation);
                timeline.publish();
            }
            self.closed.retain(|closed| closed != &id);
        }
        true
    }

    /// What a revocation drops besides windows: reads in flight, typing,
    /// evidence that is no longer visible, and, when a channel was lost,
    /// the profiles and catalog that might have come from it. Open windows
    /// then load again.
    fn revoke_access(&mut self, lost: &[String]) {
        self.epoch += 1;
        self.workflows.clear();
        self.typing.clear();
        let revoked: Vec<String> = {
            let all: Vec<Verified> = self.recent.iter().map(|(_, event)| event.clone()).collect();
            let visible: std::collections::HashSet<EventId> =
                self.visible(&all, false).iter().map(|e| e.id).collect();
            all.iter()
                .filter(|event| event.kind_u16() == kinds::PROFILE || !visible.contains(&event.id))
                .map(Verified::id_hex)
                .collect()
        };
        for id in revoked {
            self.recent.remove(&id);
        }
        self.purge_retained();
        self.activity.values.clear();
        self.activity_cancel();
        self.refresh_activity();
        if !lost.is_empty() {
            let lost: std::collections::HashSet<String> = lost.iter().cloned().collect();
            self.reader.invalidate(move |filters| {
                filters.iter().any(|filter| {
                    ["#h", "#d"].iter().any(|key| {
                        filter
                            .get(*key)
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|ids| {
                                ids.iter()
                                    .any(|id| id.as_str().is_some_and(|id| lost.contains(id)))
                            })
                    })
                })
            });
            // Names and emoji seen only in a lost channel must not stay.
            self.profiles.clear();
            self.emoji_clear();
            self.statuses_clear();
            self.members_clear();
            self.ensure_profiles(self.channel_people());
            let people: Vec<String> = self.timelines.values().flat_map(Timeline::people).collect();
            self.ensure_profiles(people);
        }
        for channel in self.open_channels() {
            self.ensure(&channel);
        }
        self.publish_activity();
    }

    /// Channels with an open view.
    fn open_channels(&self) -> Vec<String> {
        self.timelines
            .values()
            .filter(|timeline| timeline.viewers > 0)
            .map(|timeline| timeline.id.clone())
            .collect()
    }

    fn channel_people(&self) -> Vec<String> {
        let Some(discovery) = &self.discovery else {
            return Vec::new();
        };
        discovery
            .channels()
            .into_iter()
            .filter_map(|channel| channel.participants)
            .flatten()
            .collect()
    }

    /// Live interests: every joined channel plus open ones, with the open
    /// ones (selected first) set up before the rest.
    fn update_interests(&mut self) {
        let (Some(live), Some(discovery)) = (&mut self.live, &self.discovery) else {
            return;
        };
        let joined: Vec<String> = discovery
            .channels()
            .into_iter()
            .filter(|channel| !channel.cached)
            .map(|c| c.id)
            .collect();
        let mut prioritized: Vec<String> = self.selected.iter().cloned().collect();
        prioritized.extend(
            self.timelines
                .values()
                .filter(|t| t.viewers > 0 && Some(&t.id) != self.selected.as_ref())
                .map(|t| t.id.clone()),
        );
        let readable = |id: &String| discovery.can_access(id) && !discovery.is_cached(id);
        let interests: Vec<String> = joined
            .iter()
            .cloned()
            .chain(prioritized.iter().cloned())
            .filter(readable)
            .collect();
        prioritized.retain(readable);
        if let Err(error) = live.update(interests, joined, prioritized) {
            tracing::warn!(%error, "live interests refused");
        }
    }

    // ----- live -----

    fn live_update(&mut self, update: LiveUpdate) {
        match update {
            LiveUpdate::Events {
                events,
                phase,
                channel,
                ..
            } => {
                // Typing pulses never enter history, unread or caches.
                let (pulses, events): (Vec<Verified>, Vec<Verified>) = events
                    .into_iter()
                    .partition(|event| event.kind_u16() == kinds::TYPING);
                self.reminders_live(&events);
                let candidates = if phase == crate::live::Phase::Live {
                    self.incoming_candidates(&events, channel.as_deref())
                } else {
                    Vec::new()
                };
                let visible = if events.is_empty() {
                    Vec::new()
                } else {
                    let visible = self.accept(&events);
                    let connected = self.live.as_ref().is_some_and(Live::is_connected);
                    if connected && self.activity_accept(&visible) {
                        self.publish();
                    }
                    visible
                };
                if !pulses.is_empty() && self.live.as_ref().is_some_and(Live::is_connected) {
                    self.typing_accept(&pulses);
                }
                // Only what the access filter kept (`subscribeLive`).
                if phase == crate::live::Phase::Live {
                    self.feed_live(&visible, channel.as_deref());
                }
                let arrived: Vec<String> = candidates
                    .iter()
                    .filter_map(|event| event.channel().map(str::to_owned))
                    .collect();
                self.admit_incoming(candidates);
                self.sidebar_arrived(&arrived);
            }
            LiveUpdate::Presence(events) => self.presence_live(&events),
            LiveUpdate::Observer(events) => self.observed(events),
            LiveUpdate::Status => {
                self.publish_live();
                self.observer_status();
            }
            LiveUpdate::Recover => {
                // A repair hint, not evidence: re-read the roster and unread.
                self.start_scan(ScanOptions::default());
                self.reads_recover();
            }
            LiveUpdate::Established { route, channels } => {
                if matches!(route, crate::RouteKey::Profiles) {
                    // Profiles changed while we were away are re-read, and
                    // reads the disconnect cancelled are asked again.
                    for (ids, filter) in self.profiles.refresh() {
                        self.read_profiles(ids, filter);
                    }
                    let mut people = self.channel_people();
                    people.extend(self.timelines.values().flat_map(Timeline::people));
                    self.ensure_profiles(people);
                    self.activity_cancel();
                    self.refresh_activity();
                    // Sets replaced while we were away arrive with the
                    // whole catalog.
                    self.emoji_reconnect();
                    self.statuses_reconnected();
                }
                self.threads_established(&channels);
                self.established_channels(&channels);
            }
            LiveUpdate::Denied { channels, reason } => {
                for id in &channels {
                    self.deny_channel(id, &reason);
                }
                self.check_access();
                self.update_interests();
                self.reads_channels(false);
                self.publish();
            }
            LiveUpdate::AccessHint { channels } => {
                // A public preview whose route closed for revoked access
                // is unreadable until it is looked up again
                // (`suspendPreviews`, then `revalidatePreviews`).
                let suspended: Vec<String> = match &mut self.discovery {
                    Some(discovery) => channels
                        .iter()
                        .filter(|id| discovery.suspend(id))
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                if !suspended.is_empty() {
                    self.check_access();
                    self.update_interests();
                    self.reads_channels(false);
                    self.publish_live();
                    self.publish();
                    // Only the ones just revoked; an earlier failed one
                    // waits for Retry.
                    self.revalidate_previews(&suspended);
                }
                self.start_scan(ScanOptions {
                    strong: true,
                    ..Default::default()
                });
            }
            LiveUpdate::Connected => {
                self.set_live(true);
                self.observer_status();
                self.reminders_recover();
                self.inbox_reconnect();
                self.presence_connected(true);
                self.start_scan(ScanOptions::default());
                self.reads_reconnected();
            }
            LiveUpdate::Disconnected => {
                self.set_live(false);
                self.observer_status();
                self.reminders_stale();
                self.inbox_stale();
                self.workflows.interrupt();
                self.homes.clear();
                self.presence_connected(false);
                self.channels_disconnected();
                self.activity_cancel();
                self.reads_disconnected();
                self.publish_live();
            }
        }
    }

    /// Rebuilds the live view and publishes it if it changed.
    fn publish_live(&mut self) {
        let mut live = self.live.as_ref().map(Live::snapshot).unwrap_or_default();
        // A suspended preview says so until it is looked up again, over
        // any other live error (`publishLive`).
        if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| !discovery.suspended_channels().is_empty())
        {
            live.error = Some("Public preview access needs rechecking; retry live updates.".into());
        }
        let next = LiveView {
            live,
            roster: self.roster_refresh.clone(),
            heads: self.catch_up_states(),
        };
        if *self.live_view != next {
            self.live_view = Arc::new(next);
            self.publish();
        }
    }

    fn set_live(&mut self, live: bool) {
        self.snapshot.send_if_modified(|current| {
            if current.live == live {
                return false;
            }
            *current = Arc::new(SessionSnapshot {
                live,
                ..(**current).clone()
            });
            true
        });
    }

    /// Which events of `batch` may be shown (`eventVisibility`): every
    /// channel an event names must be readable, and an auxiliary event
    /// (deletion, reaction, edit, moderator delete, thread summary) also
    /// needs every target it references known and visible. `remote` also
    /// refuses channels whose membership is only saved.
    pub(super) fn visible(&self, batch: &[Verified], remote: bool) -> Vec<Verified> {
        const DEPTH: usize = 32;
        let by_id: HashMap<EventId, &Verified> = batch.iter().map(|e| (e.id, e)).collect();
        let local = self.local_events();
        let local: HashMap<EventId, &Verified> = local.iter().map(|e| (e.id, e)).collect();
        let readable = |id: &str| {
            if remote {
                self.can_read_remote(id)
            } else {
                self.discovery.as_ref().is_some_and(|d| d.can_access(id))
            }
        };
        let find = |id: &EventId| -> Option<Verified> {
            if let Some(event) = by_id.get(id).or_else(|| local.get(id)) {
                return Some((*event).clone());
            }
            let hex = id.to_hex();
            self.recent
                .peek(&hex)
                .cloned()
                .or_else(|| self.timelines.values().find_map(|t| t.find(id)))
                .or_else(|| self.thread_event(id))
                .or_else(|| {
                    self.reads
                        .as_ref()
                        .and_then(|reads| reads.unread.event(&hex).cloned())
                })
        };
        struct Walk<'a> {
            memo: HashMap<EventId, bool>,
            visiting: std::collections::HashSet<EventId>,
            readable: &'a dyn Fn(&str) -> bool,
            find: &'a dyn Fn(&EventId) -> Option<Verified>,
        }
        impl Walk<'_> {
            fn visible(&mut self, event: &Verified) -> bool {
                if let Some(known) = self.memo.get(&event.id) {
                    return *known;
                }
                if self.visiting.contains(&event.id) || self.visiting.len() >= DEPTH {
                    return false;
                }
                self.visiting.insert(event.id);
                let mut channels: Vec<&str> = event
                    .tags_named("h")
                    .filter_map(|tag| tag.get(1).map(String::as_str))
                    .filter(|id| !id.is_empty())
                    .collect();
                if matches!(
                    event.kind_u16(),
                    kinds::CHANNEL_METADATA | kinds::CHANNEL_ADMINS | kinds::CHANNEL_MEMBERS
                ) && let Some(id) = event.tag("d")
                    && !id.is_empty()
                {
                    channels.push(id);
                }
                let mut allowed = channels.iter().all(|id| (self.readable)(id));
                if allowed && kinds::AUXILIARY_KINDS.contains(&event.kind_u16()) {
                    let targets: Vec<EventId> = event
                        .tags_named("e")
                        .filter_map(|tag| tag.get(1))
                        .filter(|id| !id.is_empty())
                        .map(|id| EventId::from_hex(id).ok())
                        .collect::<Option<Vec<_>>>()
                        .unwrap_or_default();
                    let malformed = event
                        .tags_named("e")
                        .filter_map(|tag| tag.get(1))
                        .any(|id| !id.is_empty() && EventId::from_hex(id).is_err());
                    allowed = !malformed
                        && (!channels.is_empty() || !targets.is_empty())
                        && targets.iter().all(|target| {
                            (self.find)(target).is_some_and(|parent| self.visible(&parent))
                        });
                }
                self.visiting.remove(&event.id);
                self.memo.insert(event.id, allowed);
                allowed
            }
        }
        let mut walk = Walk {
            memo: HashMap::new(),
            visiting: std::collections::HashSet::new(),
            readable: &readable,
            find: &find,
        };
        batch
            .iter()
            .filter(|event| walk.visible(event))
            .cloned()
            .collect()
    }

    /// Every verified event goes through here, in the reference order:
    /// discovery, membership hints, then only what is visible to read
    /// state, unread, outbox echo, profiles, emoji, channel admins, channel
    /// tails and windows, and threads. Returns the visible events.
    fn accept(&mut self, events: &[Verified]) -> Vec<Verified> {
        let mut publish = false;
        if let Some(discovery) = &mut self.discovery {
            let mut changed = false;
            for event in events {
                if matches!(
                    event.kind_u16(),
                    kinds::CHANNEL_METADATA | kinds::CHANNEL_MEMBERS
                ) {
                    changed |= discovery.accept(event, false);
                }
            }
            if changed {
                publish = true;
                self.channels.discovery_observed = true;
                self.check_access();
                self.update_interests();
                self.reads_channels(false);
                self.save_startup();
            }
        }
        let viewer = self.identity.pubkey_hex();
        let hint = events.iter().any(|event| {
            matches!(
                event.kind_u16(),
                kinds::MEMBER_ADDED | kinds::MEMBER_REMOVED
            ) && self
                .relay
                .as_ref()
                .is_some_and(|relay| membership_hint(event, &relay.authority, &viewer).is_some())
        });
        if hint {
            self.start_scan(ScanOptions {
                strong: true,
                ..Default::default()
            });
        }
        // Ephemeral typing, telemetry and private feedback never enter
        // retained views.
        let candidates: Vec<Verified> = events
            .iter()
            .filter(|event| {
                !matches!(
                    event.kind_u16(),
                    kinds::AGENT_ACTIVITY | kinds::TYPING | PRODUCT_FEEDBACK
                )
            })
            .cloned()
            .collect();
        let visible = self.visible(&candidates, true);
        if visible.is_empty() {
            if publish {
                self.publish();
            }
            return visible;
        }
        // Completion evidence from any admitted read ends typing.
        self.typing_accept(&visible);
        for event in &visible {
            self.recent
                .set(event.id_hex(), event.clone(), event.byte_size());
        }
        publish |= self.reads_accept(&visible);
        if let Some(outbox) = &self.outbox {
            outbox.observe(&visible);
        }
        publish |= self.profiles.accept(&visible);
        publish |= self.emoji.accept(&visible);
        publish |= self.statuses.accept(&visible, unread::now_ms());
        publish |= self.admins_accept(&visible);
        self.accept_tails(&visible);
        let local = self.local_ids();
        let mut people = Vec::new();
        for timeline in self.timelines.values_mut() {
            if timeline.accept(&visible, &local) {
                timeline.publish();
                people.extend(timeline.people());
                publish = true;
            }
        }
        self.threads_receive(&visible);
        self.ensure_profiles(people);
        self.reading_arrivals(&visible);
        if publish || !self.channels_preview_current() {
            self.publish();
        }
        visible
    }

    /// Whether the published channel previews still match.
    fn channels_preview_current(&self) -> bool {
        self.channels_cache.as_ref().is_none_or(|(_, channels)| {
            channels
                .iter()
                .all(|channel| channel.preview == self.preview(&channel.id))
        })
    }

    /// The sidebar channels with their previews and activity. Summaries
    /// are rebuilt only when the discovery revision moved, and the last
    /// list is reused when nothing on it changed.
    fn published_channels(&mut self) -> Arc<[ChannelSummary]> {
        let Some(discovery) = &self.discovery else {
            self.channels_cache = None;
            return Arc::new([]);
        };
        let revision = discovery.revision();
        let (base, cached) = match &self.channels_cache {
            Some((cached, channels)) if *cached == revision => (channels.clone(), true),
            _ => (Arc::from(discovery.channels()), false),
        };
        let extras: Vec<_> = base
            .iter()
            .map(|channel| {
                (
                    self.preview(&channel.id),
                    self.activity.values.last(&channel.id),
                )
            })
            .collect();
        let current = cached
            && base.iter().zip(&extras).all(|(channel, (preview, at))| {
                channel.preview == *preview && channel.last_activity_at == *at
            });
        let channels = if current {
            base
        } else {
            base.iter()
                .zip(extras)
                .map(|(channel, (preview, at))| ChannelSummary {
                    preview,
                    last_activity_at: at,
                    ..channel.clone()
                })
                .collect()
        };
        debug_assert!(
            channels
                .iter()
                .map(|channel| ChannelSummary {
                    preview: None,
                    last_activity_at: None,
                    ..channel.clone()
                })
                .eq(discovery.channels()),
            "the channel cache missed a discovery revision"
        );
        self.channels_cache = Some((revision, channels.clone()));
        channels
    }

    // ----- timelines -----

    fn open(&mut self, channel: &str, view: watch::Sender<Arc<TimelineSnapshot>>) {
        self.closed.retain(|id| id != channel);
        let Some(timeline) = self.touch(channel) else {
            // Before the relay (or a saved snapshot) named its identity.
            self.waiting
                .entry(channel.to_string())
                .or_default()
                .push(view);
            return;
        };
        timeline.viewers += 1;
        timeline.attach(view);
        self.ensure(channel);
        self.update_interests();
        self.update_reading();
    }

    fn close(&mut self, channel: &str) {
        if let Some(views) = self.waiting.get_mut(channel) {
            views.retain(|view| !view.is_closed());
            if views.is_empty() {
                self.waiting.remove(channel);
            }
        }
        let Some(timeline) = self.timelines.get_mut(channel) else {
            return;
        };
        timeline.viewers = timeline.viewers.saturating_sub(1);
        if timeline.viewers > 0 {
            return;
        }
        self.view_reports.remove(channel);
        self.closed.push_back(channel.to_string());
        while self.closed.len() > RETAINED_CLOSED {
            if let Some(evicted) = self.closed.pop_front() {
                self.timelines.remove(&evicted);
            }
        }
        self.update_interests();
        self.update_reading();
    }

    /// Discovery may learn that an open channel is a session channel,
    /// whose replies are rows.
    fn refresh_session_windows(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        for timeline in self.timelines.values_mut() {
            timeline.set_session(discovery.is_session(&timeline.id));
            timeline.publish();
        }
    }

    /// Forgets the memory caches of one channel.
    fn channels_forget(&mut self, channel: &str) {
        self.deny_cached(channel);
    }

    // ----- writes -----

    fn write(&mut self, write: Write) -> Result<(), String> {
        let outbox = self
            .outbox
            .clone()
            .ok_or("Not connected to the relay yet")?;
        // A draft naming a code needs the catalog (`emoji.tags`).
        let content = match &write {
            Write::Send { content, .. } | Write::Edit { content, .. } => Some(content),
            _ => None,
        };
        if content.is_some_and(|content| !messages::referenced_emoji(content).is_empty()) {
            self.ensure_emoji();
        }
        let viewer = self.identity.pubkey_hex();
        let authority = self.relay.as_ref().map(|relay| relay.authority.clone());
        let author = Author {
            viewer: &viewer,
            signing_authority: authority.as_deref(),
        };
        let find = |channel: &str, id: &EventId| {
            self.timelines
                .get(channel)
                .and_then(|timeline| timeline.find(id))
                .ok_or_else(|| "Load the message first".to_string())
        };
        // Every write needs a channel the viewer may post in (`writer`).
        let channel = match &write {
            Write::Send { channel, .. }
            | Write::Edit { channel, .. }
            | Write::React { channel, .. }
            | Write::Remove { channel, .. } => Some(channel.clone()),
            Write::SendToChannel { row, .. } => Some(row.channel.to_string()),
            Write::Retry(_) | Write::Dismiss(_) => None,
        };
        if let Some(channel) = channel
            && !self
                .discovery
                .as_ref()
                .is_some_and(|d| d.can_participate(&channel))
        {
            return Err("Join the conversation before posting".into());
        }
        let emoji = self.emoji.snapshot();
        let drafts = match write {
            Write::Send {
                channel,
                content,
                mentions,
                attachments,
                thread,
            } => {
                // Mentions are checked against live membership, so none go
                // while the live connection is down (`mentionTags`).
                if !mentions.recipients.is_empty()
                    && !self.live.as_ref().is_some_and(Live::is_connected)
                {
                    return Err("Refresh channel membership before mentioning anyone".into());
                }
                if let Some(discovery) = &self.discovery {
                    crate::members::validate_mentions(
                        discovery.get(&channel).as_ref(),
                        discovery.can_participate(&channel),
                        &mentions.recipients,
                    )?;
                }
                let compose = Compose {
                    channel_id: &channel,
                    content: &content,
                    mentions: &mentions,
                    attachments: &attachments,
                    emoji: emoji.catalog(),
                    origin: &self.origin,
                };
                let draft = match thread {
                    Some((root, parent)) => {
                        messages::reply(&compose, &root.to_hex(), &parent.to_hex())
                    }
                    None => messages::message(&compose),
                };
                vec![draft.map_err(|error| error.to_string())?]
            }
            Write::Edit {
                channel,
                id,
                content,
            } => {
                // A thread reply open in its thread is found too.
                let original = find(&channel, &id)
                    .ok()
                    .or_else(|| self.find_message(&channel, &id))
                    .ok_or("Load the message first")?;
                // The folded row says where its media came from: the
                // original or the latest own edit carrying `imeta`.
                let source = self
                    .timelines
                    .get(&channel)
                    .and_then(|timeline| timeline.row(&id))
                    .and_then(|row| row.attachment_source_id)
                    .unwrap_or(id);
                let source = self
                    .find_event(&source)
                    .ok_or("Reload the message before editing its attachments.")?;
                vec![
                    messages::edit(&author, &original, &source, &content, emoji.catalog())
                        .map_err(|error| error.to_string())?,
                ]
            }
            Write::React {
                channel,
                id,
                content,
                emoji: pill,
            } => {
                let original = self
                    .find_message(&channel, &id)
                    .ok_or("Load the message first")?;
                // A palette emoji needs no ready catalog to react with.
                let custom = pill.or_else(|| {
                    content
                        .strip_prefix(':')
                        .and_then(|code| code.strip_suffix(':'))
                        .and_then(|code| emoji.get(&code.to_ascii_lowercase()))
                        .cloned()
                });
                vec![
                    messages::react(&original, &content, custom.as_ref(), emoji.catalog())
                        .map_err(|error| error.to_string())?,
                ]
            }
            Write::Remove { channel, id, agent } => {
                let target = self
                    .find_message(&channel, &id)
                    .ok_or("Load the message first")?;
                let ids = vec![id.to_hex()];
                let authorization = agent.as_deref().map(|agent_id| messages::AgentOwner {
                    agent_id,
                    owner_id: &viewer,
                });
                messages::remove(&author, &ids, |_| Some(target.event()), authorization)
                    .map_err(|error| error.to_string())?
            }
            Write::SendToChannel { row, root } => {
                if row.author.to_hex() != viewer {
                    return Err("Only your own messages can be sent to the channel".into());
                }
                let reply = self
                    .find_event(&row.id)
                    .ok_or("Load a thread reply before sending it to the channel")?;
                if crate::fold::thread_reference(&reply)
                    .is_none_or(|thread| Some(thread.root_id) != row.thread_root_id)
                    || reply.channel() != Some(&*row.channel)
                {
                    return Err("Reload the thread reply before sending it to the channel".into());
                }
                let source = self
                    .find_event(&row.attachment_source_id.unwrap_or(row.id))
                    .ok_or("Reload the message attachments before sending")?;
                let urls: Vec<String> = row.attachments.iter().map(|a| a.url.clone()).collect();
                let mentions: Vec<String> = row
                    .mentions
                    .iter()
                    .chain(&row.mention_references)
                    .map(|key| key.to_hex())
                    .collect();
                let emoji_list: Vec<messages::CustomEmoji> = row
                    .emoji
                    .iter()
                    .map(|emoji| messages::CustomEmoji {
                        shortcode: emoji.shortcode.clone(),
                        url: emoji.url.clone(),
                    })
                    .collect();
                let root_id = root.as_ref().map(|root| root.id.to_hex());
                let thread_root =
                    root.as_ref()
                        .zip(root_id.as_deref())
                        .map(|(root, id)| messages::ThreadRoot {
                            id,
                            channel_id: &root.channel,
                            content: &root.content,
                        });
                let delivery = row.delivery.as_ref().map(|delivery| match delivery.status {
                    crate::fold::Delivery::Sending => crate::outbox::Delivery::Sending,
                    crate::fold::Delivery::Accepted => crate::outbox::Delivery::Accepted,
                    crate::fold::Delivery::Unknown => crate::outbox::Delivery::Unknown,
                    crate::fold::Delivery::Failed => crate::outbox::Delivery::Failed,
                    crate::fold::Delivery::Seen => crate::outbox::Delivery::Seen,
                });
                let content = row.source_content.as_deref().unwrap_or(&row.content);
                vec![
                    messages::sent_from_thread(
                        &author,
                        &messages::SentFromThread {
                            reply: reply.event(),
                            attachment_source: source.event(),
                            content,
                            attachment_urls: &urls,
                            mentions: &mentions,
                            edited: row.edited,
                            emoji: &emoji_list,
                            delivery,
                            root: thread_root,
                        },
                    )
                    .map_err(|error| error.to_string())?,
                ]
            }
            Write::Retry(id) => return outbox.retry(&id, None).map_err(|error| error.to_string()),
            Write::Dismiss(_) => unreachable!("dismissals finish asynchronously"),
        };
        for draft in drafts {
            outbox
                .send(draft, SendOptions::default())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Any retained event: windows, open threads, recent traffic or the
    /// outbox.
    pub(super) fn find_event(&self, id: &EventId) -> Option<Verified> {
        self.timelines
            .values()
            .find_map(|timeline| timeline.find(id))
            .or_else(|| self.thread_event(id))
            .or_else(|| self.recent.peek(&id.to_hex()).cloned())
            .or_else(|| {
                self.local_events()
                    .into_iter()
                    .find(|event| event.id == *id)
            })
    }

    /// Shows the outbox's operations in their timelines.
    fn local_changed(&mut self) {
        let local = self.local_operations();
        for timeline in self.timelines.values_mut() {
            timeline.set_local(&local);
            timeline.publish();
        }
        self.threads_local(&local);
        self.track_additions();
    }

    /// The outbox's events, signed or not yet, before visibility.
    fn local_events(&self) -> Vec<Verified> {
        let Some(outbox) = &self.outbox else {
            return Vec::new();
        };
        outbox
            .visible()
            .iter()
            .filter_map(|op| match op.signed.clone() {
                Some(signed) => self.verifier.verify(signed).ok(),
                None => Verified::local(&op.event),
            })
            .collect()
    }

    fn local_ids(&self) -> std::collections::HashSet<EventId> {
        self.outbox
            .as_ref()
            .map(|outbox| outbox.visible().iter().map(|op| op.id()).collect())
            .unwrap_or_default()
    }

    /// The outbox's visible operations with their delivery state
    /// (`local()`): a send shows at once, before it is even signed.
    fn local_operations(&self) -> Vec<(Verified, LocalDelivery)> {
        let Some(outbox) = &self.outbox else {
            return Vec::new();
        };
        let operations = outbox.visible();
        let events: Vec<(Verified, &Arc<crate::outbox::Operation>)> = operations
            .iter()
            .filter(|op| op.kind() != PRODUCT_FEEDBACK)
            .filter_map(|op| {
                let event = match op.signed.clone() {
                    Some(signed) => self.verifier.verify(signed).ok()?,
                    None => Verified::local(&op.event)?,
                };
                Some((event, op))
            })
            .collect();
        let batch: Vec<Verified> = events.iter().map(|(event, _)| event.clone()).collect();
        let visible: std::collections::HashSet<EventId> =
            self.visible(&batch, false).iter().map(|e| e.id).collect();
        events
            .into_iter()
            .filter(|(event, _)| visible.contains(&event.id))
            .map(|(event, op)| {
                let status = match op.delivery {
                    crate::outbox::Delivery::Sending => crate::fold::Delivery::Sending,
                    crate::outbox::Delivery::Accepted => crate::fold::Delivery::Accepted,
                    crate::outbox::Delivery::Unknown => crate::fold::Delivery::Unknown,
                    crate::outbox::Delivery::Failed => crate::fold::Delivery::Failed,
                    crate::outbox::Delivery::Seen => crate::fold::Delivery::Seen,
                };
                (
                    event,
                    LocalDelivery {
                        status,
                        error: op.error.clone(),
                    },
                )
            })
            .collect()
    }

    // ----- profiles -----

    fn ensure_profiles(&mut self, ids: Vec<String>) {
        if self.relay.is_none() || ids.is_empty() {
            return;
        }
        let reads = self.profiles.ensure(ids.iter().map(String::as_str));
        for (ids, filter) in reads {
            self.read_profiles(ids, filter);
        }
    }

    fn read_profiles(&self, ids: Vec<String>, filter: Filter) {
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let result = reader.read(vec![filter], ReadOptions::background()).await;
            let _ = commands.send(Message::Profiles { ids, result });
        });
    }

    // ----- custom emoji -----

    /// Reads every member's emoji set at background priority, once the
    /// catalog was asked for.
    fn read_emoji(&mut self) {
        if !self.emoji_requested || self.relay.is_none() || self.emoji_reading {
            return;
        }
        self.emoji_reading = true;
        if self.emoji.loading() {
            self.publish();
        }
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        let generation = self.emoji_generation;
        self.runtime.spawn(async move {
            let result = reader
                .read(vec![catalog_filter()], ReadOptions::background())
                .await;
            let _ = commands.send(Message::EmojiRead { generation, result });
        });
    }

    /// The first use reads the catalog; later ones wait for it, and an
    /// error stays until a refresh.
    fn ensure_emoji(&mut self) {
        self.emoji_requested = true;
        if self.emoji.snapshot().status == crate::emoji::CatalogStatus::Idle {
            self.read_emoji();
        }
    }

    /// The stream was re-established: an in-flight catalog read is
    /// abandoned and a fresh one starts (`emoji.reconnect`).
    fn emoji_reconnect(&mut self) {
        self.emoji_generation += 1;
        self.emoji_reading = false;
        self.read_emoji();
    }

    /// Forgets the catalog (access change or cache clear) and reads it
    /// again.
    fn emoji_clear(&mut self) {
        self.emoji_generation += 1;
        self.emoji_reading = false;
        self.emoji = EmojiDirectory::new(self.identity.pubkey_hex());
        self.read_emoji();
    }

    // ----- output -----

    fn publish(&mut self) {
        let channels = self.published_channels();
        let unread = self.unread_counts();
        self.workflows_access();
        self.sidebar_check_heads();
        self.sidebar_state();
        let next = SessionSnapshot {
            status: self.snapshot.borrow().status.clone(),
            live: self.live.as_ref().is_some_and(Live::is_connected),
            cached: self.cached,
            restoring: self.restoring,
            live_status: self.live_view.clone(),
            relay: self.relay.clone(),
            roster: self.roster.clone(),
            channels,
            profiles: self.profiles.snapshot(),
            unread,
            presence: self.presence.snapshot(),
            typing: self.typing.snapshot().channels.clone(),
            thread_typing: self.typing.snapshot().threads.clone(),
            typing_anywhere: self.typing.snapshot().anywhere.clone(),
            hidden_dms: self.hidden_dms.clone(),
            dm_visibility: self.dm_visibility.clone(),
            emoji: self.emoji.snapshot(),
            sidebar: self.sidebar.snapshot(),
            statuses: self.statuses.snapshot(),
        };
        self.snapshot.send_if_modified(|current| {
            let same = current.status == next.status
                && current.live == next.live
                && current.cached == next.cached
                && current.relay == next.relay
                && current.roster == next.roster
                && Arc::ptr_eq(&current.profiles, &next.profiles)
                && Arc::ptr_eq(&current.unread, &next.unread)
                && Arc::ptr_eq(&current.presence, &next.presence)
                && Arc::ptr_eq(&current.typing, &next.typing)
                && Arc::ptr_eq(&current.thread_typing, &next.thread_typing)
                && Arc::ptr_eq(&current.typing_anywhere, &next.typing_anywhere)
                && Arc::ptr_eq(&current.hidden_dms, &next.hidden_dms)
                && current.dm_visibility == next.dm_visibility
                && Arc::ptr_eq(&current.emoji, &next.emoji)
                && Arc::ptr_eq(&current.sidebar, &next.sidebar)
                && Arc::ptr_eq(&current.statuses, &next.statuses)
                && Arc::ptr_eq(&current.live_status, &next.live_status)
                && *current.channels == *next.channels;
            if !same {
                // Unchanged channels keep their snapshot.
                let channels = if *current.channels == *next.channels {
                    current.channels.clone()
                } else {
                    next.channels.clone()
                };
                *current = Arc::new(SessionSnapshot { channels, ..next });
            }
            !same
        });
        self.publish_watches();
        self.inbox_publish();
        self.after_publish();
    }
}

/// Product feedback lands in a private sidecar and never in views.
const PRODUCT_FEEDBACK: u16 = 42000;

/// NIP-11 fields the client uses (spec §1.4). `self` is required: without
/// it relay-signed events cannot be trusted.
fn relay_info(info: &serde_json::Value) -> Result<RelayInfo, String> {
    let authority = info
        .get("self")
        .and_then(serde_json::Value::as_str)
        .filter(|key| is_hex64(key))
        .ok_or("Relay did not advertise its identity")?;
    let text = |key: &str| {
        info.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Ok(RelayInfo {
        name: text("name"),
        icon: info
            .get("icon")
            .and_then(serde_json::Value::as_str)
            .and_then(crate::community_icon::community_icon),
        authority: authority.to_string(),
        supports_channel_creation: info
            .get("supported_nips")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|nips| nips.iter().any(|nip| nip.as_u64() == Some(29))),
        channel_labels: info
            .get("supported_extensions")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|extensions| {
                extensions
                    .iter()
                    .any(|extension| extension.as_str() == Some("nip-cl"))
            }),
        read_state_snapshot: SnapshotDescriptor::from_nip11(info),
    })
}
