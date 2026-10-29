//! The user's communities and their sessions, as GPUI entities.

use crate::idle::Idle;
use crate::mirror::mirror;
use crate::notifications;
use crate::runtime::Runtime;
use buzz_identity::Identity;
use buzz_relay_client::{
    ChannelType, ChannelView, LeaveOutcome, PresenceMode, RelayOrigin, Session, SessionSnapshot,
    Store, TimelineSnapshot, TimelineStatus,
};
use gpui_kit::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Viewer preference holding the saved communities.
const SAVED: &str = "communities";
/// View state key for the channel last open in a community.
const SELECTED_CHANNEL: &str = "selected-channel";
/// View state key for the community's last NIP-11 icon, shown before the
/// relay answers (buzz-app saves it on the membership).
const ICON: &str = "community-icon";
/// Viewer preference holding the availability choice (`buzz-presence.v1`).
const PRESENCE_MODE: &str = "presence-mode";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    origins: Vec<String>,
    selected: Option<String>,
    /// Personal space is selected: no community, though some are saved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    personal: bool,
}

pub enum IdentityState {
    /// The saved identity is being read (`identity_restore`).
    Loading,
    Ready(Arc<Identity>),
    /// No identity is saved yet; onboarding creates or imports one.
    Missing,
    /// The saved identity could not be read; onboarding offers a retry.
    Error(String),
}

pub struct AppState {
    pub identity: IdentityState,
    pub store: Store,
    pub communities: Vec<Entity<Community>>,
    pub selected: Option<usize>,
    /// The availability the user chose, applied to every community.
    pub presence_mode: PresenceMode,
    /// The saved availability could not be read or saved (`activity.ts`).
    pub presence_error: Option<String>,
}

impl Global for AppState {}

impl AppState {
    pub fn init(cx: &mut App) {
        let path = data_dir().join("buzz.sqlite3");
        let store = Store::open(&path).unwrap_or_else(|error| {
            tracing::error!(%error, path = %path.display(), "could not open local storage; using memory");
            Store::in_memory().expect("in-memory store")
        });
        cx.set_global(Self {
            identity: IdentityState::Loading,
            store,
            communities: Vec::new(),
            selected: None,
            presence_mode: PresenceMode::Auto,
            presence_error: None,
        });
        // A development identity file is read at once, so development
        // launches open straight into the workspace.
        if dev_identity_file().is_some() {
            Self::restored(identity::restore(), cx);
        } else {
            Self::restore(cx);
        }
    }

    /// Reads the saved identity off the UI thread, as buzz-app's
    /// `identity_restore` does; the keyring may wait on an unlock prompt.
    /// Also the "Retry secure storage access" action.
    pub fn restore(cx: &mut App) {
        if matches!(cx.global::<Self>().identity, IdentityState::Ready(_)) {
            return;
        }
        cx.global_mut::<Self>().identity = IdentityState::Loading;
        let read = cx.background_spawn(async { identity::restore() });
        cx.spawn(async move |cx| {
            let result = read.await;
            cx.update(|cx| Self::restored(result, cx));
        })
        .detach();
    }

    fn restored(result: Result<Option<Identity>, String>, cx: &mut App) {
        match result {
            Ok(Some(identity)) => Self::set_identity(identity, cx),
            Ok(None) => cx.global_mut::<Self>().identity = IdentityState::Missing,
            Err(error) => cx.global_mut::<Self>().identity = IdentityState::Error(error),
        }
    }

    /// Adopts a restored, created or imported identity and opens its saved
    /// communities.
    pub fn set_identity(identity: buzz_identity::Identity, cx: &mut App) {
        crate::perf::step(crate::perf::STARTUP, "identity");
        let identity = Arc::new(identity);
        let store = cx.global::<Self>().store.clone();
        let saved = cx.global::<Self>().saved(&identity.pubkey_hex());
        let (mode, presence_error) = load_presence_mode(&store, &identity.pubkey_hex());
        let communities: Vec<Entity<Community>> = saved
            .origins
            .iter()
            .filter_map(|origin| RelayOrigin::parse(origin).ok())
            .map(|origin| Community::new(origin, identity.clone(), store.clone(), mode, cx))
            .collect();
        let state = cx.global_mut::<Self>();
        state.selected = saved
            .selected
            .and_then(|selected| saved.origins.iter().position(|o| *o == selected))
            .or((!communities.is_empty() && !saved.personal).then_some(0));
        state.communities = communities;
        state.identity = IdentityState::Ready(identity);
        state.presence_mode = mode;
        state.presence_error = presence_error;
        crate::perf::step(crate::perf::STARTUP, "sessions");
    }

    fn saved(&self, viewer: &str) -> Saved {
        self.store
            .viewer_pref(viewer, SAVED)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    fn save(&self, cx: &App) {
        let IdentityState::Ready(identity) = &self.identity else {
            return;
        };
        let origins: Vec<String> = self
            .communities
            .iter()
            .map(|community| community.read(cx).origin.to_string())
            .collect();
        let saved = Saved {
            selected: self.selected.and_then(|index| origins.get(index).cloned()),
            personal: self.selected.is_none() && !origins.is_empty(),
            origins,
        };
        let json = serde_json::to_string(&saved).expect("saved communities serialize");
        if let Err(error) = self
            .store
            .set_viewer_pref(&identity.pubkey_hex(), SAVED, &json)
        {
            tracing::error!(%error, "could not save communities");
        }
    }

    /// Adds a community by relay URL and selects it.
    pub fn add_community(input: &str, cx: &mut App) -> Result<(), String> {
        let origin = RelayOrigin::parse(input).map_err(|error| error.to_string())?;
        let state = cx.global::<Self>();
        let IdentityState::Ready(identity) = &state.identity else {
            return Err("Set up your identity first".into());
        };
        if let Some(index) = state
            .communities
            .iter()
            .position(|community| community.read(cx).origin == origin)
        {
            cx.global_mut::<Self>().selected = Some(index);
            cx.global::<Self>().save(cx);
            return Ok(());
        }
        let (identity, store) = (identity.clone(), state.store.clone());
        let community = Community::new(origin, identity, store, state.presence_mode, cx);
        let state = cx.global_mut::<Self>();
        state.communities.push(community);
        state.selected = Some(state.communities.len() - 1);
        cx.global::<Self>().save(cx);
        Ok(())
    }

    /// Moves the community at `from` to `to` in the rail, keeping the
    /// same one selected. The order is saved with the communities.
    pub fn move_community(from: usize, to: usize, cx: &mut App) {
        let state = cx.global_mut::<Self>();
        let len = state.communities.len();
        if from >= len || to >= len || from == to {
            return;
        }
        let community = state.communities.remove(from);
        state.communities.insert(to, community);
        state.selected = state.selected.map(|index| moved_index(index, from, to));
        cx.global::<Self>().save(cx);
    }

    pub fn select(index: usize, cx: &mut App) {
        let state = cx.global_mut::<Self>();
        if index < state.communities.len() {
            state.selected = Some(index);
            cx.global::<Self>().save(cx);
        }
    }

    /// Selects Personal space: no community.
    pub fn select_personal(cx: &mut App) {
        cx.global_mut::<Self>().selected = None;
        cx.global::<Self>().save(cx);
    }

    /// Sets the availability every community publishes, and remembers it.
    pub fn set_presence_mode(mode: PresenceMode, cx: &mut App) {
        cx.global_mut::<Self>().presence_mode = mode;
        let state = cx.global::<Self>();
        for community in &state.communities {
            community.read(cx).session.set_presence_mode(mode);
        }
        let IdentityState::Ready(identity) = &state.identity else {
            return;
        };
        let value = match mode {
            PresenceMode::Auto => "\"auto\"",
            PresenceMode::Away => "\"away\"",
            PresenceMode::Offline => "\"offline\"",
        };
        let saved = state
            .store
            .set_viewer_pref(&identity.pubkey_hex(), PRESENCE_MODE, value);
        let error = saved.err().map(|error| {
            tracing::warn!(%error, "could not save the availability choice");
            "Could not save your status. It applies to this window until you close it.".to_string()
        });
        cx.global_mut::<Self>().presence_error = error;
    }

    /// Leaves a community (NIP-43, relay.md §1.8). On success the
    /// community is dropped from this device and, unless the relay says
    /// access was revoked, its local data is purged. Any failure keeps the
    /// membership so the user can retry.
    /// Also whether any saved data outlived the purge (`residue`).
    pub fn leave(
        community: Entity<Community>,
        cx: &mut App,
    ) -> Task<Result<(LeaveOutcome, bool), String>> {
        let session = community.read(cx).session.clone();
        cx.spawn(async move |cx| {
            let outcome = session.leave().await?;
            let purge = outcome != LeaveOutcome::Revoked;
            let purged = cx.update(|cx| Self::remove(&community, purge, cx));
            let residue = !matches!(purged.await, Ok(Ok(())));
            Ok((outcome, residue))
        })
    }

    /// Drops a community from this device: another community becomes
    /// selected, the session stops, then its local data is purged.
    /// Resolves once the purge has run: `Err` when saved data remains.
    fn remove(
        community: &Entity<Community>,
        purge: bool,
        cx: &mut App,
    ) -> tokio::task::JoinHandle<Result<(), ()>> {
        let id = community.entity_id();
        let state = cx.global_mut::<Self>();
        let Some(index) = state
            .communities
            .iter()
            .position(|other| other.entity_id() == id)
        else {
            return Runtime::handle(cx).spawn(async { Ok(()) });
        };
        state.communities.remove(index);
        let remaining = state.communities.len();
        state.selected = match state.selected {
            _ if remaining == 0 => None,
            Some(selected) if selected > index => Some(selected - 1),
            // Leaving the selected community lands on Personal space.
            Some(selected) if selected == index => None,
            other => other,
        };
        cx.global::<Self>().save(cx);
        let (session, partition) = {
            let community = community.read(cx);
            (community.session.clone(), community.partition())
        };
        if purge {
            crate::ui::emoji_picker::forget_quick_reactions(&partition, cx);
        }
        let store = cx.global::<Self>().store.clone();
        Runtime::handle(cx).spawn(async move {
            session.dispose().await;
            if purge && let Err(error) = store.purge(&partition) {
                tracing::error!(%error, "could not purge the community's local data");
                return Err(());
            }
            Ok(())
        })
    }

    /// Sign out's commit point (`IdentityHost::close`): every session
    /// stops, so nothing more is signed with this key before Buzz restarts
    /// and the next launch removes it.
    pub fn close_sessions(cx: &mut App) {
        let sessions: Vec<_> = cx
            .global::<Self>()
            .communities
            .iter()
            .map(|community| community.read(cx).session.clone())
            .collect();
        let runtime = Runtime::handle(cx);
        for session in sessions {
            runtime.spawn(async move { session.dispose().await });
        }
    }

    pub fn current(cx: &App) -> Option<Entity<Community>> {
        let state = cx.global::<Self>();
        state
            .selected
            .and_then(|index| state.communities.get(index).cloned())
    }
}

pub struct Community {
    pub origin: RelayOrigin,
    pub session: Arc<Session>,
    pub snapshot: Arc<SessionSnapshot>,
    /// The channel last opened, which notifications skip while the window
    /// is focused.
    pub reading: Option<String>,
    /// Whose presence the session follows.
    presence_demand: Vec<String>,
    /// People an open view (the members dialog) also follows.
    presence_extra: Vec<String>,
    /// People plugins follow, by watch.
    presence_plugins: std::collections::BTreeMap<u64, Vec<String>>,
    /// Agents whose messages the open timeline shows, for their badges.
    presence_rows: Vec<String>,
    /// People open profile panels show, counted per panel.
    presence_profiles: Vec<String>,
    /// The icon last saved for the rail.
    icon: Option<String>,
    store: Store,
    _watch: Task<()>,
    _notifications: Option<Task<()>>,
}

impl Community {
    fn new(
        origin: RelayOrigin,
        identity: Arc<Identity>,
        store: Store,
        presence_mode: PresenceMode,
        cx: &mut App,
    ) -> Entity<Self> {
        let session = Arc::new(Session::open(
            &Runtime::handle(cx),
            origin.clone(),
            identity.clone(),
            store.clone(),
        ));
        let icon = store
            .view_state(
                &buzz_relay_client::Partition::new(&origin, session.viewer()),
                ICON,
            )
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<String>(&json).ok())
            .and_then(|icon| buzz_relay_client::community_icon::community_icon(&icon));
        if cx.try_global::<Idle>().is_some_and(|idle| idle.idle) {
            session.set_idle(true);
        }
        if cx.try_global::<Idle>().is_some_and(|idle| !idle.visible) {
            session.set_visible(false);
        }
        if presence_mode != PresenceMode::Auto {
            session.set_presence_mode(presence_mode);
        }
        let community = cx.new(|cx| {
            let snapshot = session.snapshot();
            let current = snapshot.borrow().clone();
            Self {
                _watch: mirror(snapshot, cx, |this: &mut Self, value| {
                    this.snapshot = value;
                    this.follow_presence();
                    this.remember_icon();
                }),
                origin,
                session,
                snapshot: current,
                reading: None,
                presence_demand: Vec::new(),
                presence_extra: Vec::new(),
                presence_plugins: Default::default(),
                presence_rows: Vec::new(),
                presence_profiles: Vec::new(),
                icon,
                store,
                _notifications: None,
            }
        });
        let notifications = notifications::listen(&community, cx);
        community.update(cx, |community, _| {
            community._notifications = Some(notifications);
        });
        crate::agent_activity::observer::attach(&community, identity.clone(), cx);
        crate::ui::settings::capture_agent_activity(&community, identity, cx);
        community
    }

    /// Follows the viewer's presence, for the account menu, and the
    /// presence and status of everyone with a one-to-one DM, for the
    /// sidebar's avatar dots and status emoji.
    fn follow_presence(&mut self) {
        let viewer = self.session.viewer().to_string();
        let mut demand: Vec<String> = std::iter::once(viewer)
            .chain(
                self.snapshot
                    .channels
                    .iter()
                    .filter(|channel| {
                        channel.channel_type == Some(ChannelType::Dm) && !channel.archived
                    })
                    .filter_map(|channel| match channel.participants.as_deref() {
                        Some([other]) => Some(other.clone()),
                        _ => None,
                    }),
            )
            .collect();
        for person in self
            .presence_extra
            .iter()
            .chain(&self.presence_rows)
            .chain(&self.presence_profiles)
            .chain(self.presence_plugins.values().flatten())
        {
            if !demand.contains(person) {
                demand.push(person.clone());
            }
        }
        if demand != self.presence_demand {
            self.presence_demand = demand.clone();
            self.session.watch_presence(demand);
        }
    }

    /// Follows the presence of agents shown in the timeline, replacing the
    /// previous set.
    pub fn set_presence_rows(&mut self, people: Vec<String>) {
        if self.presence_rows != people {
            self.presence_rows = people;
            self.follow_presence();
        }
    }

    /// Also follows these people's presence, replacing the previous extra
    /// set.
    /// Follows `people` for a plugin's watch `id`, or stops with none.
    pub fn set_presence_plugin(&mut self, id: u64, people: Vec<String>) {
        if people.is_empty() {
            self.presence_plugins.remove(&id);
        } else {
            self.presence_plugins.insert(id, people);
        }
        self.follow_presence();
    }

    pub fn set_presence_extra(&mut self, people: Vec<String>) {
        self.presence_extra = people;
        self.follow_presence();
    }

    /// Follows one more person's presence for a profile panel, until
    /// [`Self::release_profile_presence`].
    pub fn hold_profile_presence(&mut self, person: &str) {
        self.presence_profiles.push(person.to_string());
        self.follow_presence();
    }

    pub fn release_profile_presence(&mut self, person: &str) {
        if let Some(index) = self.presence_profiles.iter().position(|p| p == person) {
            self.presence_profiles.remove(index);
            self.follow_presence();
        }
    }

    /// Opens `channel`. `seed` is what it showed when it closed a moment
    /// ago: it stands in for the session's loading state, so the rows show
    /// at once rather than a frame later, when the session answers.
    pub fn open_channel(
        &mut self,
        channel: &str,
        seed: Option<Arc<TimelineSnapshot>>,
        cx: &mut App,
    ) -> Entity<ChannelModel> {
        let view = self.session.open_channel(channel);
        self.session.select(Some(channel.to_string()));
        self.reading = Some(channel.to_string());
        cx.new(|cx| {
            let current = view.snapshot.borrow().clone();
            let seed = seed.filter(|seed| {
                current.status == TimelineStatus::Loading && *seed.channel == *channel
            });
            Self::channel_model(view, current, seed, cx)
        })
    }

    fn channel_model(
        view: ChannelView,
        current: Arc<TimelineSnapshot>,
        seed: Option<Arc<TimelineSnapshot>>,
        cx: &mut Context<ChannelModel>,
    ) -> ChannelModel {
        ChannelModel {
            _watch: mirror(
                view.snapshot.clone(),
                cx,
                |this: &mut ChannelModel, value| {
                    // The session's first value is the loading state the
                    // seed stands in for.
                    if std::mem::take(&mut this.seeded) && value.status == TimelineStatus::Loading {
                        return;
                    }
                    this.snapshot = value
                },
            ),
            view,
            seeded: seed.is_some(),
            snapshot: seed.unwrap_or(current),
        }
    }

    pub fn partition(&self) -> buzz_relay_client::Partition {
        buzz_relay_client::Partition::new(&self.origin, self.session.viewer())
    }

    /// The channel the user last had open here.
    pub fn remembered_channel(&self, cx: &App) -> Option<String> {
        let json = cx
            .global::<AppState>()
            .store
            .view_state(&self.partition(), SELECTED_CHANNEL)
            .ok()??;
        serde_json::from_str(&json).ok()
    }

    pub fn remember_channel(&self, channel: &str, cx: &App) {
        let json = serde_json::to_string(channel).expect("strings serialize");
        let saved = cx.global::<AppState>().store.set_view_state(
            &self.partition(),
            SELECTED_CHANNEL,
            Some(&json),
        );
        if let Err(error) = saved {
            tracing::warn!(%error, "could not remember the selected channel");
        }
    }

    /// The rail icon: the relay's NIP-11 icon, else the one saved from an
    /// earlier connection; `None` shows initials.
    pub fn icon(&self) -> Option<&str> {
        self.snapshot
            .relay
            .as_ref()
            .and_then(|relay| relay.icon.as_deref())
            .or(self.icon.as_deref())
    }

    /// Saves a new NIP-11 icon for the next launch.
    fn remember_icon(&mut self) {
        let Some(icon) = self
            .snapshot
            .relay
            .as_ref()
            .and_then(|relay| relay.icon.clone())
        else {
            return;
        };
        if self.icon.as_ref() == Some(&icon) {
            return;
        }
        let json = serde_json::to_string(&icon).expect("strings serialize");
        if let Err(error) = self
            .store
            .set_view_state(&self.partition(), ICON, Some(&json))
        {
            tracing::warn!(%error, "could not save the community icon");
        }
        self.icon = Some(icon);
    }

    /// Display name: the relay's NIP-11 name, else its host.
    pub fn name(&self) -> String {
        self.snapshot
            .relay
            .as_ref()
            .and_then(|relay| relay.name.clone())
            .unwrap_or_else(|| {
                self.origin
                    .websocket()
                    .split("://")
                    .nth(1)
                    .unwrap_or_default()
                    .to_string()
            })
    }
}

/// One open channel. Dropping the entity releases the session's view.
pub struct ChannelModel {
    pub view: ChannelView,
    pub snapshot: Arc<TimelineSnapshot>,
    /// `snapshot` is a seed until the session's first value arrives.
    seeded: bool,
    _watch: Task<()>,
}

impl ChannelModel {
    pub fn load_older(&self) {
        self.view.load_older();
    }
}

/// The saved availability; an unreadable one is Auto with the error the
/// account menu shows.
fn load_presence_mode(store: &Store, viewer: &str) -> (PresenceMode, Option<String>) {
    match store.viewer_pref(viewer, PRESENCE_MODE) {
        Ok(saved) => (
            match saved.as_deref() {
                Some("\"away\"") => PresenceMode::Away,
                Some("\"offline\"") => PresenceMode::Offline,
                _ => PresenceMode::Auto,
            },
            None,
        ),
        Err(error) => {
            tracing::warn!(%error, "could not read the availability choice");
            (
                PresenceMode::Auto,
                Some("Could not read your saved status. Changes apply to this window.".into()),
            )
        }
    }
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("buzz-gpui")
}

/// For development on machines without a keyring, `BUZZ_DEV_NSEC_FILE`
/// names a file that stands in for the keyring entry.
pub fn dev_identity_file() -> Option<PathBuf> {
    std::env::var_os("BUZZ_DEV_NSEC_FILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// The human key's store: the keyring entry buzz-app also uses, or the
/// development file in its place. With the file, the keyring is never
/// read, written or deleted.
pub mod identity {
    use super::dev_identity_file;
    use buzz_identity::{Identity, IdentityError};
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};

    /// The saved identity; `None` when none is saved.
    pub fn restore() -> Result<Option<Identity>, String> {
        let Some(path) = dev_identity_file() else {
            return match buzz_identity::load().map_err(|error| error.to_string())? {
                Some(identity) => Ok(Some(identity)),
                None => Ok(adopt_once(
                    adopted_marker().as_deref(),
                    buzz_identity::adopt_buzz_app,
                )),
            };
        };
        tracing::warn!(path = %path.display(), "using a development identity file");
        read_file(&path).map_err(|error| error.to_string())
    }

    /// Present once this app has looked for buzz-app's identity. It sits
    /// beside the sign-out markers, outside app data, so neither signing
    /// out nor erasing brings buzz-app's key back.
    fn adopted_marker() -> Option<PathBuf> {
        dirs::data_dir().map(|data| {
            data.join(format!(
                ".buzz-gpui--{}.identity-adopted",
                buzz_identity::BUZZ_APP_SERVICE
            ))
        })
    }

    /// buzz-app's identity, copied into this app's entry on the first
    /// launch without one. A storage failure (denied, busy) is retried next
    /// launch; until then the user can still import or create a key. A
    /// malformed buzz-app entry will not fix itself, so it is recorded as
    /// read rather than retried, which on macOS would prompt every launch.
    fn adopt_once(
        marker: Option<&Path>,
        adopt: impl FnOnce() -> Result<Option<Identity>, IdentityError>,
    ) -> Option<Identity> {
        let marker = marker?;
        if marker.exists() {
            return None;
        }
        let adopted = match adopt() {
            Ok(adopted) => adopted,
            Err(IdentityError::Malformed) => {
                tracing::warn!("buzz-app's identity is malformed; not reading it again");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "could not read buzz-app's identity");
                return None;
            }
        };
        if let Err(error) = mark_adopted(marker) {
            tracing::warn!(%error, "could not record that buzz-app's identity was read");
        }
        if let Some(identity) = &adopted {
            tracing::info!(pubkey = %identity.pubkey_hex(), "using buzz-app's identity");
        }
        adopted
    }

    fn mark_adopted(marker: &Path) -> std::io::Result<()> {
        std::fs::write(marker, b"")
    }

    /// Records the adoption before sign-out deletes the key. Without it, a
    /// user whose key was never adopted (it predates buzz-app's move, or was
    /// pasted in) would get buzz-app's key copied back at the next launch.
    /// A failed write fails the removal, so the sign-out stays pending and
    /// the next launch retries.
    fn remove_keychain_key(
        marker: Option<&Path>,
        forget: impl FnOnce() -> Result<(), IdentityError>,
    ) -> Result<(), String> {
        if let Some(marker) = marker {
            mark_adopted(marker).map_err(|error| {
                tracing::error!(%error, "could not record that buzz-app's identity was read");
                error.to_string()
            })?;
        }
        forget().map_err(|error| error.to_string())
    }

    /// Saves a new random identity, or the imported `nsec`.
    pub fn save(nsec: Option<&str>) -> Result<Identity, String> {
        let Some(path) = dev_identity_file() else {
            return match nsec {
                Some(nsec) => buzz_identity::import(nsec),
                None => buzz_identity::create(),
            }
            .map_err(|error| error.to_string());
        };
        if read_file(&path)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err(IdentityError::AlreadySaved.to_string());
        }
        let identity = match nsec {
            Some(nsec) => Identity::from_nsec(nsec).map_err(|error| error.to_string())?,
            None => Identity::ephemeral(),
        };
        std::fs::write(&path, identity.nsec().as_bytes())
            .map_err(|_| IdentityError::Unavailable.to_string())?;
        Ok(identity)
    }

    /// Sign out's last step at the next launch: deletes the saved key and
    /// confirms a fresh read finds nothing. An absent key is success.
    pub fn remove() -> Result<(), String> {
        let Some(path) = dev_identity_file() else {
            return remove_keychain_key(adopted_marker().as_deref(), buzz_identity::forget);
        };
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        }
    }

    /// An absent or empty file is no identity.
    fn read_file(path: &Path) -> Result<Option<Identity>, IdentityError> {
        match std::fs::read_to_string(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(_) => Err(IdentityError::Unavailable),
            Ok(text) if text.trim().is_empty() => Ok(None),
            Ok(text) => Identity::from_nsec(&text)
                .map(Some)
                .map_err(|_| IdentityError::Malformed),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;

        fn marker_in(dir: &tempfile::TempDir) -> PathBuf {
            dir.path().join("identity-adopted")
        }

        #[test]
        fn a_recorded_adoption_is_not_repeated() {
            let dir = tempfile::tempdir().unwrap();
            let marker = marker_in(&dir);
            mark_adopted(&marker).unwrap();
            let called = Cell::new(false);
            let adopted = adopt_once(Some(&marker), || {
                called.set(true);
                Ok(Some(Identity::ephemeral()))
            });
            assert!(adopted.is_none());
            assert!(!called.get());
        }

        #[test]
        fn adoption_is_recorded_whether_or_not_buzz_app_has_a_key() {
            let dir = tempfile::tempdir().unwrap();
            let marker = marker_in(&dir);
            let human = Identity::ephemeral();
            let expected = human.pubkey_hex();
            let adopted = adopt_once(Some(&marker), move || Ok(Some(human))).unwrap();
            assert_eq!(adopted.pubkey_hex(), expected);
            assert!(marker.exists());

            let empty = tempfile::tempdir().unwrap();
            let marker = marker_in(&empty);
            assert!(adopt_once(Some(&marker), || Ok(None)).is_none());
            assert!(marker.exists());
        }

        #[test]
        fn a_storage_error_is_retried_but_malformed_data_is_not() {
            let dir = tempfile::tempdir().unwrap();
            let marker = marker_in(&dir);
            for error in [IdentityError::Denied, IdentityError::Busy] {
                assert!(adopt_once(Some(&marker), || Err(error)).is_none());
                assert!(!marker.exists());
            }
            assert!(adopt_once(Some(&marker), || Err(IdentityError::Malformed)).is_none());
            assert!(marker.exists());
        }

        #[test]
        fn sign_out_records_the_adoption_before_the_key_is_deleted() {
            let dir = tempfile::tempdir().unwrap();
            let marker = marker_in(&dir);
            let forgot = Cell::new(false);
            remove_keychain_key(Some(&marker), || {
                assert!(
                    marker.exists(),
                    "the marker must exist before the key is deleted"
                );
                forgot.set(true);
                Ok(())
            })
            .unwrap();
            assert!(forgot.get());
            // The next launch finds no key and does not adopt buzz-app's.
            let adopted = adopt_once(Some(&marker), || Ok(Some(Identity::ephemeral())));
            assert!(adopted.is_none());
        }

        #[test]
        fn sign_out_fails_and_keeps_the_key_when_the_marker_cannot_be_written() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("missing").join("identity-adopted");
            let forgot = Cell::new(false);
            let result = remove_keychain_key(Some(&marker), || {
                forgot.set(true);
                Ok(())
            });
            assert!(result.is_err());
            assert!(!forgot.get());
        }
    }
}

/// Where the item at `index` ends up when the one at `from` moves to `to`.
fn moved_index(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

#[cfg(test)]
mod move_tests {
    use super::moved_index;
    use core::prelude::v1::test;

    #[test]
    fn the_selection_follows_its_community() {
        // [a, b, c, d]: move b (1) to 3 gives [a, c, d, b].
        assert_eq!(moved_index(1, 1, 3), 3);
        assert_eq!(moved_index(0, 1, 3), 0);
        assert_eq!(moved_index(2, 1, 3), 1);
        assert_eq!(moved_index(3, 1, 3), 2);
        // Move d (3) to 0 gives [d, a, b, c].
        assert_eq!(moved_index(3, 3, 0), 0);
        assert_eq!(moved_index(0, 3, 0), 1);
        assert_eq!(moved_index(2, 3, 0), 3);
    }
}
