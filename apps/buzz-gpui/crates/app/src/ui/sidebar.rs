//! The channel sidebar (ui.md §3; `ChannelSidebar.tsx`): page rows, then
//! Starred, saved groups, Channels, Forums and Direct messages arranged by
//! the viewer's saved preferences, with the row and section menus, moves
//! by drag and drop, and the footer notices of failed saves.
//!
//! One [`ChannelSidebar`] entity per community keeps what the user is
//! doing here (open menus, dialogs, collapsed sections, the scroll
//! position). Preferences and their writes belong to the session
//! ([`SessionSnapshot::sidebar`]).

mod dialogs;
pub(super) mod menus;
mod rows;

pub(crate) use rows::{own_agents, working_agents, working_badge};

use super::message::display_name;
use super::pages::{self, Page, Placement};
use crate::community::{AppState, Community};
use buzz_relay_client::lifecycle::{Action, LifecyclePermissions};
use buzz_relay_client::sidebar::{SidebarSection, listed, sidebar_sections};
use buzz_relay_client::sidebar_store::{Destination, PreferencesStatus, SidebarState};
use buzz_relay_client::{
    ChannelSummary, ChannelType, ChannelUnread, Profiles, RosterStatus, SessionSnapshot,
    SessionStatus, StatusWatch,
};
use buzz_ui::{ActiveTheme, Button, ButtonSize, NavigationItem, Toast, ToastTone, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use menus::header_actions;

pub struct SidebarProps {
    pub page: Page,
    pub selected: Option<String>,
    pub on_select_channel: ChannelHandler,
    pub on_select_page: PageHandler,
    /// Section "+" actions (§3.5). Channel creation is `None` when the relay
    /// does not support it.
    pub on_create_channel: Option<ActionHandler>,
    pub on_new_message: ActionHandler,
    /// The viewer left, hid or deleted a channel from its row menu.
    pub on_channel_removed: ChannelIdHandler,
}

pub type ActionHandler = Arc<dyn Fn(&mut Window, &mut App)>;
pub type ChannelHandler = Arc<dyn Fn(&ChannelSummary, &mut Window, &mut App)>;
pub type ChannelIdHandler = Arc<dyn Fn(&str, &mut Window, &mut App)>;
pub type PageHandler = Arc<dyn Fn(Page, &mut Window, &mut App)>;

/// The reveal waits this long for preferences, names, activity and
/// unread counts (`SIDEBAR_REVEAL_BUDGET_MS`).
const REVEAL_BUDGET: Duration = Duration::from_millis(1500);
/// `useToastNotification`'s timeout.
const TOAST_DURATION: Duration = Duration::from_secs(4);

/// DM label: the other participants' names, or "Notes to self".
pub fn channel_label(channel: &ChannelSummary, profiles: &Profiles) -> SharedString {
    match (&channel.channel_type, &channel.participants) {
        (Some(ChannelType::Dm), Some(participants)) if participants.is_empty() => {
            "Notes to self".into()
        }
        (Some(ChannelType::Dm), Some(participants)) => participants
            .iter()
            .map(|p| display_name(profiles, p).to_string())
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
        _ => channel.name.clone().into(),
    }
}

/// The roster as the sidebar arranges it: DMs named after their
/// participants (`useChannelLabels`). "Recent" sorting reads the
/// session's `last_activity_at`, which [`ChannelSidebar`] asks for while a
/// section sorts that way.
fn labelled(snapshot: &SessionSnapshot) -> Vec<ChannelSummary> {
    snapshot
        .channels
        .iter()
        // Conversations placed in Me leave Messages.
        .filter(|channel| !snapshot.sidebar.me_channels.contains(&channel.id))
        .map(|channel| {
            let mut channel = channel.clone();
            if channel.channel_type == Some(ChannelType::Dm) {
                channel.name = channel_label(&channel, &snapshot.profiles).to_string();
            }
            channel
        })
        .collect()
}

/// DMs hidden by the relay (30622) or removed from Messages here.
fn hidden_dms(snapshot: &SessionSnapshot) -> HashSet<String> {
    snapshot
        .hidden_dms
        .iter()
        .chain(&snapshot.sidebar.hidden_dms)
        .cloned()
        .collect()
}

/// The sections in the order [`ChannelSidebar`] draws them.
fn sections<'a>(
    channels: &'a [ChannelSummary],
    sidebar: &'a SidebarState,
    hidden: &HashSet<String>,
) -> Vec<SidebarSection<'a>> {
    sidebar_sections(channels, sidebar.data.as_ref(), hidden)
}

/// The first sidebar row other than `except`, in the order the sidebar
/// draws them; where the selection goes when a channel closes.
pub fn first_row(snapshot: &SessionSnapshot, except: &str) -> Option<ChannelSummary> {
    let channels = labelled(snapshot);
    let hidden = hidden_dms(snapshot);
    let id = sections(&channels, &snapshot.sidebar, &hidden)
        .iter()
        .flat_map(|section| section.rows.iter())
        .find(|channel| channel.id != except)
        .map(|channel| channel.id.clone())?;
    snapshot.channels.iter().find(|c| c.id == id).cloned()
}

/// The unread state of the channels the sidebar lists (here or in Me),
/// which is all the rail and the tray count: archived and hidden channels
/// and hidden DMs keep their read state but show nowhere.
pub fn listed_unread(
    snapshot: &SessionSnapshot,
) -> impl Iterator<Item = (&ChannelSummary, ChannelUnread)> {
    let hidden = hidden_dms(snapshot);
    snapshot
        .channels
        .iter()
        .filter(move |channel| listed(channel, &hidden))
        .filter_map(|channel| Some((channel, *snapshot.unread.channels.get(&channel.id)?)))
}

/// Draws the community's sidebar, making its state on first use.
pub fn render(
    community: &Entity<Community>,
    props: SidebarProps,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let sidebar = ChannelSidebar::for_community(community, window, cx);
    let key = PropsKey::of(&props, cx);
    let (changed, overlay) = sidebar.update(cx, |sidebar, _| {
        let changed = sidebar.props_key.as_ref() != Some(&key);
        sidebar.props_key = Some(key);
        sidebar.props = Some(props);
        (changed, sidebar.overlay_open())
    });
    // Hundreds of rows are costly to lay out, so the sidebar draws from
    // its last frame unless something it shows changed: its own state and
    // the entities it read notify, and new props draw it afresh here. An
    // open menu or dialog draws every frame, as overlays expect.
    if changed || overlay {
        sidebar.into_any_element()
    } else {
        AnyView::from(sidebar)
            .cached(StyleRefinement::default().size_full())
            .into_any_element()
    }
}

/// What the sidebar shows from outside its own state and the entities it
/// reads; a change redraws it.
#[derive(PartialEq)]
struct PropsKey {
    page: Page,
    selected: Option<String>,
    can_create: bool,
    pages: Vec<(Page, &'static str)>,
    hosts: usize,
}

impl PropsKey {
    fn of(props: &SidebarProps, cx: &App) -> Self {
        Self {
            page: props.page,
            selected: props.selected.clone(),
            can_create: props.on_create_channel.is_some(),
            pages: pages::placed(Placement::Sidebar, cx)
                .into_iter()
                .map(|spec| (spec.page, spec.label))
                .collect(),
            hosts: cx
                .try_global::<crate::hosts::Hosts>()
                .map_or(0, |hosts| hosts.hosts.len()),
        }
    }
}

/// Where the list showed at the last paint: its visible bounds and its
/// scroll offset.
type Painted = (Bounds<Pixels>, Pixels);

/// One sidebar per community, kept while the community is.
#[derive(Default)]
struct Sidebars(HashMap<EntityId, Entity<ChannelSidebar>>);

impl Global for Sidebars {}

/// View state key for the channel sidebar (`buzz-view.v1:[scope,
/// "channel-sidebar"]`), per community and viewer.
const SIDEBAR_VIEW: &str = "channel-sidebar";

/// The channel sidebar's saved view: its width, the collapsed sections and
/// where the list was scrolled (`useSidebarView.ts`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SidebarView {
    /// `None` is the default width.
    pub width: Option<f32>,
    /// Section keys collapsed by the user.
    pub collapsed: Vec<String>,
    pub scroll_top: f32,
    /// The conversation whose row the saved position shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl SidebarView {
    /// The saved view for `community`, or the default one.
    pub fn load(community: &Community, cx: &App) -> Self {
        cx.global::<AppState>()
            .store
            .view_state(&community.partition(), SIDEBAR_VIEW)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, community: &Community, cx: &App) {
        let json = serde_json::to_string(self).expect("the view serializes");
        let saved = cx.global::<AppState>().store.set_view_state(
            &community.partition(),
            SIDEBAR_VIEW,
            Some(&json),
        );
        if let Err(error) = saved {
            tracing::warn!(%error, "could not save the sidebar view");
        }
    }

    /// Changes the saved view in place, keeping fields others own (the
    /// width).
    fn update(community: &Community, cx: &App, change: impl FnOnce(&mut Self)) {
        let mut view = Self::load(community, cx);
        change(&mut view);
        view.save(community, cx);
    }
}

/// A row menu: where it is anchored, and whether its Move submenu is out.
pub(super) struct RowMenu {
    channel: ChannelSummary,
    section: String,
    at: Point<Pixels>,
    move_open: bool,
    /// Lifecycle permissions, read when the menu opens.
    permissions: Option<Result<LifecyclePermissions, String>>,
    /// The header's Channel actions menu opened it (no lifecycle items).
    surface: bool,
}

impl RowMenu {
    pub(super) fn new(channel: ChannelSummary, section: String, at: Point<Pixels>) -> Self {
        Self {
            channel,
            section,
            at,
            move_open: false,
            permissions: None,
            surface: false,
        }
    }

    /// Opened from the header's "Move channel": only the Move submenu.
    pub(super) fn header_move(mut self) -> Self {
        self.surface = true;
        self.move_open = true;
        self
    }
}

pub(super) struct SectionMenu {
    key: String,
    sort_open: bool,
}

/// "Mark as Read/Unread" in a menu: saving, or failed.
#[derive(Clone, Default)]
pub(super) struct ReadAction {
    pending: bool,
    error: Option<SharedString>,
}

/// A mute shown before the relay confirms it (`useOptimisticMute`).
#[derive(Clone)]
pub(super) struct MuteIntent {
    name: String,
    muted: bool,
    pending: bool,
    error: Option<String>,
    generation: u64,
}

/// A row being dragged to another section.
#[derive(Clone)]
pub(super) struct DraggedChannel {
    id: String,
    section: String,
    name: SharedString,
    icon: buzz_ui::IconName,
}

pub struct ChannelSidebar {
    community: Entity<Community>,
    props: Option<SidebarProps>,
    props_key: Option<PropsKey>,
    collapsed: Vec<String>,
    scroll: ScrollHandle,
    /// The scroll offset this sidebar last set or saw.
    scroll_seen: Option<f32>,
    /// The saved position has been put back.
    scroll_restored: bool,
    /// The conversation whose row should be scrolled into view.
    reveal: Option<String>,
    /// The selection last seen, to reveal it once when it changes.
    selected: Option<String>,
    /// A save of the scroll position is scheduled.
    saving_scroll: bool,
    /// The location the saved position shows; it is not revealed again.
    location: Option<String>,
    /// Row bounds from the last paint, for reveals and keyboard menus.
    bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Row bounds from the last paint, taken at the start of a render to
    /// pick the rows drawn in full ([`Self::row_shown`]).
    painted: HashMap<String, Bounds<Pixels>>,
    /// The list's visible bounds and scroll offset at the last paint.
    viewport: Rc<std::cell::Cell<Option<Painted>>>,
    focus: HashMap<String, FocusHandle>,
    /// The first reveal happened (`useSidebarStartup`).
    revealed: bool,
    roster_since: Option<Instant>,
    row_menu: Option<RowMenu>,
    section_menu: Option<SectionMenu>,
    read_action: ReadAction,
    dialog: Option<dialogs::Dialog>,
    mutes: HashMap<String, MuteIntent>,
    mute_generation: u64,
    toast: Option<(SharedString, ToastTone, usize)>,
    toasts: usize,
    dragging: Option<String>,
    /// Follows the one-to-one DM peers' statuses (`useUserStatus`).
    status_watch: Option<StatusWatch>,
    status_demand: Vec<String>,
    /// What `Session::track_channel_activity` was last told.
    tracking_activity: Option<bool>,
    _tasks: Vec<Task<()>>,
    _observe: Subscription,
}

impl ChannelSidebar {
    fn for_community(
        community: &Entity<Community>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let live: HashSet<EntityId> = cx
            .global::<AppState>()
            .communities
            .iter()
            .map(Entity::entity_id)
            .collect();
        let sidebars = cx.default_global::<Sidebars>();
        sidebars.0.retain(|id, _| live.contains(id));
        if let Some(sidebar) = sidebars.0.get(&community.entity_id()) {
            return sidebar.clone();
        }
        let sidebar = cx.new(|cx| Self::new(community.clone(), window, cx));
        cx.global_mut::<Sidebars>()
            .0
            .insert(community.entity_id(), sidebar.clone());
        sidebar
    }

    /// The sidebar of `community`, if it was drawn.
    fn of(community: &Entity<Community>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Sidebars>()?
            .0
            .get(&community.entity_id())
            .cloned()
    }

    fn new(community: Entity<Community>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = SidebarView::load(community.read(cx), cx);
        let observe = cx.observe(&community, |this, _, cx| {
            this.follow_statuses(cx);
            this.follow_activity(cx);
            cx.notify();
        });
        Self {
            community,
            props: None,
            props_key: None,
            collapsed: view.collapsed,
            scroll: ScrollHandle::new(),
            scroll_seen: None,
            scroll_restored: false,
            reveal: None,
            selected: None,
            saving_scroll: false,
            location: view.location.clone(),
            bounds: Rc::default(),
            painted: HashMap::new(),
            viewport: Rc::default(),
            focus: HashMap::new(),
            revealed: false,
            roster_since: None,
            row_menu: None,
            section_menu: None,
            read_action: ReadAction::default(),
            dialog: None,
            mutes: HashMap::new(),
            mute_generation: 0,
            toast: None,
            toasts: 0,
            dragging: None,
            status_watch: None,
            status_demand: Vec::new(),
            tracking_activity: None,
            _tasks: Vec::new(),
            _observe: observe,
        }
        .with_saved_scroll(view.scroll_top)
    }

    fn with_saved_scroll(mut self, top: f32) -> Self {
        self.scroll_seen = Some(-top.max(0.));
        self
    }

    fn session(&self, cx: &App) -> Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    // ----- view state -----

    /// Opens or closes a section (or a parent's sessions), saved per
    /// community.
    fn toggle(&mut self, key: &str, open: bool, cx: &mut Context<Self>) {
        if self.collapsed.iter().any(|k| k == key) != open {
            return;
        }
        if open {
            self.collapsed.retain(|k| k != key);
        } else {
            self.collapsed.push(key.to_owned());
        }
        let collapsed = self.collapsed.clone();
        SidebarView::update(self.community.read(cx), cx, |view| {
            view.collapsed = collapsed
        });
        cx.notify();
    }

    fn is_open(&self, key: &str) -> bool {
        !self.collapsed.iter().any(|k| k == key)
    }

    /// Restores the saved position once the sections exist, follows user
    /// scrolling, and scrolls a newly selected row into view.
    fn manage_scroll(&mut self, ready: bool, window: &mut Window, cx: &mut Context<Self>) {
        let offset = self.scroll.offset().y.as_f32();
        if !ready {
            return;
        }
        if !self.scroll_restored {
            self.scroll_restored = true;
            let saved = self.scroll_seen.unwrap_or(0.);
            self.scroll.set_offset(point(px(0.), px(saved)));
            self.scroll_seen = Some(saved);
            window.request_animation_frame();
            return;
        }
        if self.scroll_seen != Some(offset) {
            // The user scrolled: the position now belongs to the current
            // selection, and wins over a pending reveal.
            self.scroll_seen = Some(offset);
            self.reveal = None;
            self.location = self.selected.clone();
            self.save_scroll(cx);
        }
        let Some(target) = self.reveal.clone() else {
            return;
        };
        let Some(row) = self.bounds.borrow().get(&target).copied() else {
            return;
        };
        let viewport = self.scroll.bounds();
        if viewport.size.height <= px(0.) {
            return;
        }
        let (top, bottom) = (viewport.top(), viewport.bottom());
        let delta = if row.top() >= top && row.bottom() <= bottom {
            px(0.)
        } else if row.bottom() <= top || row.top() >= bottom {
            row.top() - top - (viewport.size.height - row.size.height) / 2.
        } else if row.top() < top {
            row.top() - top
        } else {
            row.bottom() - bottom
        };
        let max = self.scroll.max_offset().y;
        let next = (px(offset) - delta).clamp(-max, px(0.));
        self.scroll.set_offset(point(px(0.), next));
        self.scroll_seen = Some(next.as_f32());
        self.reveal = None;
        self.location = Some(target);
        self.save_scroll(cx);
        window.request_animation_frame();
    }

    /// Unread conversations scrolled out of view, from where rows were
    /// last painted (`SidebarUnread`).
    fn unread_edges(
        &self,
        sections: &[SidebarSection<'_>],
        snapshot: &SessionSnapshot,
    ) -> super::sidebar_unread::Edges {
        let bounds = self.bounds.borrow();
        let mut rows = Vec::new();
        for section in sections {
            let open = self.is_open(&section.key);
            for channel in &section.rows {
                let unread = snapshot.unread.channel(&channel.id);
                if !unread.unread && unread.threads == 0 {
                    continue;
                }
                let anchor = match open {
                    true => bounds.get(&channel.id),
                    false => bounds.get(&format!("section:{}", section.key)),
                };
                let Some(anchor) = anchor else {
                    continue;
                };
                // A one-to-one direct message lends the pill its face.
                let face = match channel.participants.as_deref() {
                    Some([other]) if channel.channel_type == Some(ChannelType::Dm) => {
                        let profile = snapshot.profiles.get(other);
                        Some(super::sidebar_unread::Face {
                            name: channel_label(channel, &snapshot.profiles),
                            picture: profile.and_then(|p| p.picture.clone()),
                            agent: profile.is_some_and(|p| p.is_agent),
                        })
                    }
                    _ => None,
                };
                rows.push(super::sidebar_unread::Seen {
                    channel: channel.id.clone(),
                    bounds: *anchor,
                    face,
                });
            }
        }
        super::sidebar_unread::split(&rows, self.scroll.bounds())
    }

    /// Opens the conversation's section if it is collapsed, then scrolls
    /// its row into view without selecting it.
    fn reveal_unread(&self, cx: &mut Context<Self>) -> super::sidebar_unread::OnReveal {
        let weak = cx.entity().downgrade();
        Rc::new(move |channel, window, cx| {
            let channel = channel.to_string();
            let next = weak.clone();
            let _ = weak.update(cx, |this, cx| {
                let snapshot = this.community.read(cx).snapshot.clone();
                let channels = labelled(&snapshot);
                let hidden = hidden_dms(&snapshot);
                let closed = sections(&channels, &snapshot.sidebar, &hidden)
                    .into_iter()
                    .filter(|section| !this.is_open(&section.key))
                    .find(|section| section.rows.iter().any(|row| row.id == channel))
                    .map(|section| section.key.clone());
                if let Some(key) = closed {
                    this.toggle(&key, true, cx);
                }
                this.reveal = Some(channel);
                cx.notify();
                // A section opened here lays out its rows in this frame:
                // look again in the next. (`request_animation_frame` is for
                // drawing, not for a click.)
                window.on_next_frame(move |_, cx| {
                    let _ = next.update(cx, |_, cx| cx.notify());
                });
            });
        })
    }

    /// Saves the scroll position shortly after it settles.
    fn save_scroll(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.saving_scroll, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.saving_scroll = false;
                let top = -this.scroll_seen.unwrap_or(0.);
                let location = this.location.clone();
                SidebarView::update(this.community.read(cx), cx, |view| {
                    view.scroll_top = top;
                    view.location = location;
                });
            });
        })
        .detach();
    }

    /// The first reveal waits for preferences, names, activity and the
    /// unread sample, at most [`REVEAL_BUDGET`] after the roster arrives.
    fn startup_ready(&mut self, snapshot: &SessionSnapshot, cx: &mut Context<Self>) -> bool {
        // Saved conversations show at once (`restored`).
        if self.revealed || snapshot.channels.iter().any(|channel| channel.cached) {
            self.revealed = true;
            return true;
        }
        let roster = matches!(
            snapshot.roster,
            RosterStatus::Ready | RosterStatus::Error(_)
        );
        if !roster {
            return false;
        }
        let since = *self.roster_since.get_or_insert_with(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REVEAL_BUDGET).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
            Instant::now()
        });
        let sidebar = &snapshot.sidebar;
        let preferences =
            sidebar.data.is_some() || matches!(sidebar.status, PreferencesStatus::Error(_));
        let recent = sidebar
            .data
            .as_ref()
            .is_some_and(buzz_relay_client::sidebar::needs_activity);
        // Names come with the live connection; "Recent" activity and the
        // unread counts with the startup unread sample.
        let activity = !recent || snapshot.unread.observed;
        let settled = preferences && snapshot.live && activity && snapshot.unread.observed;
        if settled || since.elapsed() >= REVEAL_BUDGET {
            self.revealed = true;
        }
        self.revealed
    }

    // ----- recent activity -----

    /// Channel activity is read only while some section sorts by Recent
    /// (`refreshChannelActivity`).
    fn follow_activity(&mut self, cx: &mut Context<Self>) {
        let wanted = self
            .community
            .read(cx)
            .snapshot
            .sidebar
            .data
            .as_ref()
            .is_some_and(buzz_relay_client::sidebar::needs_activity);
        if self.tracking_activity.replace(wanted) != Some(wanted) {
            self.session(cx).track_channel_activity(wanted);
        }
    }

    // ----- statuses of one-to-one DM peers -----

    fn follow_statuses(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.community.read(cx).snapshot.clone();
        let mut peers: Vec<String> = snapshot
            .channels
            .iter()
            .filter(|c| c.channel_type == Some(ChannelType::Dm) && !c.archived)
            .filter_map(|c| match c.participants.as_deref() {
                Some([other]) => Some(other.clone()),
                _ => None,
            })
            .collect();
        peers.sort();
        peers.dedup();
        if peers == self.status_demand {
            return;
        }
        // The new watch is taken before the old one goes, so peers in both
        // keep their statuses.
        self.status_watch = Some(self.session(cx).watch_statuses(&peers));
        self.status_demand = peers;
    }

    // ----- actions -----

    fn show_toast(&mut self, text: &str, tone: ToastTone, cx: &mut Context<Self>) {
        self.toasts += 1;
        let generation = self.toasts;
        self.toast = Some((text.to_owned().into(), tone, generation));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .toast
                    .as_ref()
                    .is_some_and(|(_, _, g)| *g == generation)
                {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn close_menus(&mut self, cx: &mut Context<Self>) {
        self.row_menu = None;
        self.section_menu = None;
        self.read_action = ReadAction::default();
        cx.notify();
    }

    /// Moves a channel; the session shows it at once. A failure shows in
    /// the footer with Retry move.
    fn move_channel(&mut self, channel: &str, destination: Destination, cx: &mut Context<Self>) {
        self.close_menus(cx);
        // The row goes to its new section; keep that section open.
        let key = match &destination {
            Destination::Starred => "starred".to_owned(),
            Destination::Section(Some(id)) => format!("group:{id}"),
            Destination::Create(new) => format!("group:{}", new.id),
            Destination::Section(None) => {
                let dm = self
                    .community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .any(|c| c.id == channel && c.channel_type == Some(ChannelType::Dm));
                if dm { "dms" } else { "channels" }.to_owned()
            }
        };
        self.toggle(&key, true, cx);
        let session = self.session(cx);
        let channel = channel.to_owned();
        cx.spawn(async move |_, _| {
            let _ = session.sidebar_move(&channel, destination).await;
        })
        .detach();
    }

    fn retry_move(&mut self, channel: &str, cx: &mut Context<Self>) {
        let session = self.session(cx);
        let channel = channel.to_owned();
        cx.spawn(async move |_, _| {
            let _ = session.sidebar_retry_move(&channel).await;
        })
        .detach();
    }

    fn set_sort(
        &mut self,
        key: &str,
        mode: buzz_relay_client::sidebar::SortMode,
        cx: &mut Context<Self>,
    ) {
        let group = match key.strip_prefix("group:") {
            Some(id) => format!("section:{id}"),
            None => key.to_owned(),
        };
        let session = self.session(cx);
        cx.spawn(async move |_, _| {
            let _ = session.sidebar_sort(&group, mode).await;
        })
        .detach();
        self.close_menus(cx);
    }

    /// Mute or unmute; the menu shows the intent until it is confirmed,
    /// a failure becomes a notice with Retry.
    fn change_mute(&mut self, channel: &str, name: &str, muted: bool, cx: &mut Context<Self>) {
        self.mute_generation += 1;
        let generation = self.mute_generation;
        self.mutes.insert(
            channel.to_owned(),
            MuteIntent {
                name: name.to_owned(),
                muted,
                pending: true,
                error: None,
                generation,
            },
        );
        self.close_menus(cx);
        let session = self.session(cx);
        let id = channel.to_owned();
        cx.spawn(async move |this, cx| {
            let result = session.sidebar_mute(&id, muted).await;
            let _ = this.update(cx, |this, cx| {
                // A newer click owns the row now.
                if this
                    .mutes
                    .get(&id)
                    .is_none_or(|i| i.generation != generation)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.mutes.remove(&id);
                    }
                    Err(error) => {
                        if let Some(intent) = this.mutes.get_mut(&id) {
                            intent.pending = false;
                            intent.error = Some(error);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn muted(&self, channel: &str, cx: &App) -> bool {
        match self.mutes.get(channel) {
            Some(intent) if intent.pending => intent.muted,
            _ => self
                .community
                .read(cx)
                .snapshot
                .sidebar
                .data
                .as_ref()
                .is_some_and(|data| data.muted.iter().any(|id| id == channel)),
        }
    }

    /// Mark as Read / Unread: the menu says "Saving…" until it is done,
    /// then closes (`useChannelReadAction`).
    fn run_read(
        &mut self,
        channel: &str,
        unread: bool,
        close: Option<menus::Close>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.read_action = ReadAction {
            pending: true,
            error: None,
        };
        cx.notify();
        let session = self.session(cx);
        let id = channel.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let result = if unread {
                session.mark_channel_read(&id).await
            } else {
                session.mark_channel_unread(&id).await
            };
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.close_menus(cx);
                        if let Some(close) = close {
                            close(window, cx);
                        }
                    }
                    Err(error) => {
                        this.read_action = ReadAction {
                            pending: false,
                            error: Some(error.into()),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_link(&mut self, channel: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(menus::channel_link(channel)));
        self.close_menus(cx);
        self.show_toast("Link copied.", ToastTone::Success, cx);
    }

    fn hide_dm(&mut self, channel: &str, cx: &mut Context<Self>) {
        self.close_menus(cx);
        self.session(cx).hide_dm(channel);
    }

    fn open_row_menu(&mut self, menu: RowMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.section_menu = None;
        self.read_action = ReadAction::default();
        let id = menu.channel.id.clone();
        let lifecycle = !menu.surface && !menu.channel.archived && !menu.channel.read_only;
        self.row_menu = Some(menu);
        if lifecycle {
            let session = self.session(cx);
            cx.spawn_in(window, async move |this, cx| {
                let permissions = session.channel_permissions(&id).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(menu) = &mut this.row_menu
                        && menu.channel.id == id
                    {
                        menu.permissions = Some(permissions);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn choose_lifecycle(
        &mut self,
        channel: ChannelSummary,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menus(cx);
        self.dialog = Some(dialogs::Dialog::lifecycle(channel, action, window, cx));
        cx.notify();
    }
}

impl ChannelSidebar {
    /// Records the list's visible bounds and scroll offset as painted, for
    /// [`Self::row_shown`].
    fn viewport_probe(&self) -> impl IntoElement {
        let (viewport, scroll) = (self.viewport.clone(), self.scroll.clone());
        canvas(
            move |bounds, _, _| viewport.set(Some((bounds, scroll.offset().y))),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }
}

impl ChannelSidebar {
    /// A menu, dialog or toast is up.
    fn overlay_open(&self) -> bool {
        self.row_menu.is_some()
            || self.section_menu.is_some()
            || self.dialog.is_some()
            || self.toast.is_some()
    }
}

impl Render for ChannelSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(props) = self.props.take() else {
            return div().into_any_element();
        };
        let element = self.render_sidebar(&props, window, cx);
        self.props = Some(props);
        element
    }
}

impl ChannelSidebar {
    fn render_sidebar(
        &mut self,
        props: &SidebarProps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let snapshot = self.community.read(cx).snapshot.clone();
        if crate::perf::on() {
            probe_snapshot(&snapshot);
        }
        if self.dragging.is_some() && !cx.has_active_drag() {
            self.dragging = None;
        }
        // The menu follows its row; it closes when the row moves away.
        if let Some(menu) = &self.row_menu
            && !menu.surface
        {
            let channels = labelled(&snapshot);
            let hidden = hidden_dms(&snapshot);
            let present = sections(&channels, &snapshot.sidebar, &hidden)
                .iter()
                .any(|s| s.key == menu.section && s.rows.iter().any(|r| r.id == menu.channel.id));
            if !present {
                self.row_menu = None;
            }
        }
        // A new selection is revealed once, unless the saved position
        // already shows it.
        if self.selected != props.selected {
            self.selected = props.selected.clone();
            if self.selected.is_some() && self.selected != self.location {
                self.reveal = self.selected.clone();
            }
        }

        let page_rows = pages::placed(Placement::Sidebar, cx)
            .into_iter()
            .map(|spec| {
                let select_page = props.on_select_page.clone();
                let page = spec.page;
                let mut row = NavigationItem::new(
                    SharedString::from(format!("page-{}", page.key())),
                    spec.label,
                )
                .sidebar()
                .icon(spec.icon)
                .icon_stroke(2.5)
                .selected(props.page == page)
                .on_click(move |_, window, cx| select_page(page, window, cx));
                if let Some(image) = pages::image(page, cx) {
                    row = row.leading(image);
                }
                if let Some(badge) = spec.badge.and_then(|badge| badge(cx)) {
                    row = row.trailing(badge);
                }
                row
            })
            .collect::<Vec<_>>();
        let navigation = (!page_rows.is_empty()).then(|| page_list(page_rows));

        // Before the relay is ready: the page rows and a status line. A
        // saved snapshot shows its sidebar instead.
        let not_ready = match &snapshot.status {
            _ if snapshot.cached => None,
            SessionStatus::Connecting => {
                Some((SharedString::from("Connecting to your relay…"), false))
            }
            SessionStatus::Failed(error) => Some((error.clone().into(), true)),
            SessionStatus::Ready => None,
        };
        if let Some((message, retry)) = not_ready {
            let session = self.session(cx);
            return frame()
                .child(
                    scrollport().child(list().children(navigation).child(empty(message, &c)).when(
                        retry,
                        |this| {
                            this.child(
                                div().flex().px(rems(1.)).child(
                                    Button::new("retry-channels", "Retry channels")
                                        .on_click(move |_, _, _| session.retry()),
                                ),
                            )
                        },
                    )),
                )
                .into_any_element();
        }

        let ready = self.startup_ready(&snapshot, cx);
        let channels = labelled(&snapshot);
        let hidden = hidden_dms(&snapshot);
        let sidebar = snapshot.sidebar.clone();
        let sections = if ready {
            sections(&channels, &sidebar, &hidden)
        } else {
            Vec::new()
        };
        self.manage_scroll(ready && snapshot.roster == RosterStatus::Ready, window, cx);
        let edges = self.unread_edges(&sections, &snapshot);
        self.painted = std::mem::take(&mut *self.bounds.borrow_mut());
        let index: HashMap<&str, &ChannelSummary> = snapshot
            .channels
            .iter()
            .map(|channel| (channel.id.as_str(), channel))
            .collect();

        if ready && !sections.is_empty() {
            crate::perf::shown(crate::perf::STARTUP, "sidebar");
            crate::perf::shown(crate::perf::SWITCH, "sidebar");
        }
        let mut list = list().track_scroll(&self.scroll).children(navigation);
        for (at, section) in sections.iter().enumerate() {
            list = list.child(self.section(at, section, &snapshot, &index, props, window, cx));
        }
        if !ready {
            list = list.child(empty("Loading your sidebar…".into(), &c));
        }
        if let RosterStatus::Error(error) = &snapshot.roster {
            list = list.child(empty(error.clone().into(), &c));
        }
        if ready && snapshot.roster == RosterStatus::Ready && snapshot.channels.is_empty() {
            list = list.child(empty("No channels yet.".into(), &c));
        }

        let notices = self.notices(&sections, &snapshot, ready, cx);
        let menu = self.row_menu_overlay(&snapshot, props, window, cx);
        let dialog = self.dialog_overlay(&snapshot, props, window, cx);
        let toast = self.toast.as_ref().map(|(text, tone, generation)| {
            super::toasts::place(
                window,
                Toast::new(("sidebar-toast", *generation), text.clone())
                    .tone(*tone)
                    .on_dismiss(cx.listener(|this, _, _, cx| {
                        this.toast = None;
                        cx.notify();
                    })),
            )
        });
        frame()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && (this.row_menu.is_some() || this.section_menu.is_some())
                {
                    cx.stop_propagation();
                    this.close_menus(cx);
                }
            }))
            .child(
                scrollport()
                    .child(list)
                    .children(super::sidebar_unread::pills(
                        &edges,
                        &self.community.read(cx).session.media().clone(),
                        self.reveal_unread(cx),
                        cx,
                    ))
                    .child(self.viewport_probe()),
            )
            .children(notices)
            .children(menu)
            .children(dialog)
            .children(toast)
            .into_any_element()
    }

    /// Failed saves and preference errors below the list (§3.9).
    fn notices(
        &self,
        sections: &[SidebarSection<'_>],
        snapshot: &SessionSnapshot,
        ready: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = *cx.colors();
        let sidebar = &snapshot.sidebar;
        let title_of = |group: &str| {
            let key = match group.strip_prefix("section:") {
                Some(id) => format!("group:{id}"),
                None => group.to_owned(),
            };
            sections
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.title.clone())
                .unwrap_or_else(|| "this section".into())
        };
        let notice = |id: SharedString| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .gap(rems(0.25))
                .p(rems(0.5))
                .text_caption()
                .text_color(c.text_subtle)
                .border_t_1()
                .border_color(c.border_standard)
        };
        let small =
            |id: SharedString, label: &'static str| Button::new(id, label).size(ButtonSize::Sm);
        let mut notices = Vec::new();
        if snapshot.cached && matches!(snapshot.status, SessionStatus::Failed(_)) {
            notices.push(
                notice("offline".into())
                    .child("Offline · Showing saved conversations.")
                    .child(
                        div().child(
                            small("retry-connection".into(), "Retry connection")
                                .on_click(cx.listener(|this, _, _, cx| this.session(cx).retry())),
                        ),
                    )
                    .into_any_element(),
            );
        }
        // The relay's hidden-DM list could not be read (`refreshVisibility`).
        if !snapshot.cached
            && matches!(
                snapshot.dm_visibility,
                buzz_relay_client::session::DmVisibilityStatus::Error(_)
            )
        {
            notices.push(
                notice("dm-visibility-error".into())
                    .text_color(c.text_danger)
                    .child("Hidden conversations could not be refreshed.")
                    .child(div().child(
                        small("retry-dm-visibility".into(), "Retry hidden conversations").on_click(
                            cx.listener(|this, _, _, cx| this.session(cx).refresh_dm_visibility()),
                        ),
                    ))
                    .into_any_element(),
            );
        }
        for failure in &sidebar.sort_errors {
            let (group, mode) = (failure.group.clone(), failure.mode);
            let retry_group = group.clone();
            notices.push(
                notice(format!("sort-error-{group}").into())
                    .child(format!(
                        "Couldn’t save the sort order for {}. {}",
                        title_of(&group),
                        failure.error
                    ))
                    .child(
                        div()
                            .flex()
                            .gap(rems(0.5))
                            .child(
                                small(format!("retry-sort-{group}").into(), "Retry sort").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        let key = match retry_group.strip_prefix("section:") {
                                            Some(id) => format!("group:{id}"),
                                            None => retry_group.clone(),
                                        };
                                        this.set_sort(&key, mode, cx)
                                    }),
                                ),
                            )
                            .child(
                                small(format!("dismiss-sort-{group}").into(), "Dismiss").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.session(cx).sidebar_dismiss_sort(&group)
                                    }),
                                ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        for (id, intent) in self.mutes.iter().filter(|(_, i)| !i.pending) {
            let (retry_id, dismiss_id) = (id.clone(), id.clone());
            let (name, muted) = (intent.name.clone(), intent.muted);
            notices.push(
                div()
                    .p(rems(0.5))
                    .child(
                        Toast::new(
                            SharedString::from(format!("mute-error-{id}")),
                            format!(
                                "Couldn’t {} {}",
                                if muted { "mute" } else { "unmute" },
                                intent.name
                            ),
                        )
                        .description(
                            intent
                                .error
                                .clone()
                                .unwrap_or_else(|| "Please try again.".into()),
                        )
                        .tone(ToastTone::Warning)
                        .action(small(format!("retry-mute-{id}").into(), "Retry").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.change_mute(&retry_id, &name, muted, cx)
                            }),
                        ))
                        .action(
                            small(format!("dismiss-mute-{id}").into(), "Dismiss").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.mutes.remove(&dismiss_id);
                                    cx.notify();
                                }),
                            ),
                        ),
                    )
                    .into_any_element(),
            );
        }
        for failed in sidebar.moves.iter().filter(|m| !m.pending) {
            let channel = failed.intent.channel_id.clone();
            let name = snapshot
                .channels
                .iter()
                .find(|c| c.id == channel)
                .map(|c| channel_label(c, &snapshot.profiles).to_string())
                .unwrap_or_else(|| "this channel".into());
            let (retry, dismiss) = (channel.clone(), channel.clone());
            notices.push(
                notice(format!("move-error-{channel}").into())
                    .child(format!(
                        "Couldn’t save the move for {name}. {}",
                        failed.error.clone().unwrap_or_default()
                    ))
                    .child(
                        "The previous placement is shown. A partial save may already exist on \
                         the relay.",
                    )
                    .when(!sidebar.writable, |this| {
                        this.child("Refresh saved sidebar preferences before retrying this move.")
                    })
                    .child(
                        div()
                            .flex()
                            .gap(rems(0.5))
                            .child(
                                small(format!("retry-move-{channel}").into(), "Retry move")
                                    .disabled(!sidebar.writable)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_move(&retry, cx)
                                    })),
                            )
                            .child(
                                small(format!("dismiss-move-{channel}").into(), "Dismiss")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.session(cx).sidebar_dismiss_move(&dismiss)
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if !sidebar.cached
            && ready
            && let PreferencesStatus::Error(_) = sidebar.status
        {
            notices.push(
                div()
                    .p(rems(0.5))
                    .child(
                        Toast::new(
                            "preferences-error",
                            "Saved sidebar preferences couldn’t refresh",
                        )
                        .description("Your conversations are still available.")
                        .tone(ToastTone::Warning)
                        .action(
                            small("retry-preferences".into(), "Retry").on_click(
                                cx.listener(|this, _, _, cx| this.session(cx).refresh_sidebar()),
                            ),
                        ),
                    )
                    .into_any_element(),
            );
        }
        notices
    }
}

/// The channel sidebar of the personal space (no community): its page
/// rows and "Choose a community to see channels.", in the same frame,
/// scrollport and row insets as a community's, so switching spaces moves
/// nothing (`SidebarList`, #945).
pub fn personal(rows: Vec<NavigationItem>, cx: &App) -> AnyElement {
    let c = *cx.colors();
    frame()
        .child(
            scrollport().child(
                list()
                    .child(page_list(rows))
                    .child(empty("Choose a community to see channels.".into(), &c)),
            ),
        )
        .into_any_element()
}

/// The page rows above the channels.
fn page_list(rows: Vec<NavigationItem>) -> Div {
    rows::row_list().mb(rems(0.75)).children(rows)
}

/// Holds the list and what floats over it (the unread pills).
fn scrollport() -> Div {
    div().relative().flex_1().min_h_0()
}

/// The scrolling list. It reaches into the frame's side padding, keeping
/// the padding inside it, so the unread pill at the panel's edge is not
/// clipped.
fn list() -> Stateful<Div> {
    div()
        .id("sidebar")
        .flex()
        .flex_col()
        .h_full()
        .mx(rems(-0.5))
        .px(rems(0.5))
        .overflow_y_scroll()
}

/// The sidebar's frame: the shell's navigation column sets its width.
fn frame() -> Stateful<Div> {
    div()
        .id("channel-sidebar")
        .flex()
        .flex_col()
        .size_full()
        .px(rems(0.5))
        .py(rems(0.5))
}

/// `.empty`: 32px 16px of padding, 14px `text-subtle`.
fn empty(message: SharedString, c: &buzz_ui::Colors) -> Div {
    div()
        .px(rems(1.))
        .py(rems(2.))
        .text_body_sm()
        .text_color(c.text_subtle)
        .child(message)
}

/// Times what the shown community's sidebar waits for at startup.
fn probe_snapshot(snapshot: &SessionSnapshot) {
    use crate::perf::{STARTUP, SWITCH, shown};
    let reached = [
        (snapshot.cached, "snapshot.cached"),
        (
            snapshot.channels.iter().any(|c| c.cached),
            "snapshot.cached_channels",
        ),
        (snapshot.live, "snapshot.live"),
        (snapshot.roster == RosterStatus::Ready, "snapshot.roster"),
        (snapshot.sidebar.data.is_some(), "snapshot.preferences"),
        (snapshot.unread.observed, "snapshot.unread"),
    ];
    for (reached, step) in reached {
        if reached {
            shown(STARTUP, step);
            shown(SWITCH, step);
        }
    }
}

/// A snapshot of `channels` with `unread` rows, for tests of what the
/// rail and the tray count.
#[cfg(test)]
pub(crate) fn unread_snapshot(
    channels: Vec<ChannelSummary>,
    unread: &[(&str, ChannelUnread)],
    sidebar: SidebarState,
) -> SessionSnapshot {
    SessionSnapshot {
        status: SessionStatus::Ready,
        live: true,
        cached: false,
        restoring: false,
        live_status: Default::default(),
        relay: None,
        roster: RosterStatus::Ready,
        channels: channels.into(),
        profiles: Default::default(),
        unread: std::sync::Arc::new(buzz_relay_client::UnreadCounts {
            channels: unread
                .iter()
                .map(|(id, row)| (id.to_string(), *row))
                .collect(),
            ..Default::default()
        }),
        presence: Default::default(),
        typing: Default::default(),
        thread_typing: Default::default(),
        typing_anywhere: Default::default(),
        sidebar: std::sync::Arc::new(sidebar),
        hidden_dms: std::sync::Arc::new([]),
        dm_visibility: Default::default(),
        emoji: Default::default(),
        statuses: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::sidebar::{SavedSection, SidebarPreferences};
    use core::prelude::v1::test;

    fn channel(id: &str, name: &str, kind: ChannelType) -> ChannelSummary {
        ChannelSummary {
            id: id.into(),
            name: name.into(),
            channel_type: Some(kind),
            participants: (kind == ChannelType::Dm).then(|| vec!["peer".to_string()]),
            ..Default::default()
        }
    }

    #[test]
    fn link_encodes_the_channel_id() {
        assert_eq!(menus::channel_link("a b/c"), "buzz://channel/a%20b%2Fc");
        assert_eq!(menus::channel_link("x-y_z.~"), "buzz://channel/x-y_z.~");
    }

    #[test]
    fn first_row_follows_the_sections() {
        let channels = vec![
            channel("a", "alpha", ChannelType::Stream),
            channel("b", "beta", ChannelType::Stream),
        ];
        let sidebar = SidebarState {
            data: Some(SidebarPreferences {
                starred: vec!["b".into()],
                sections: vec![SavedSection {
                    id: "g".into(),
                    name: "G".into(),
                    icon: None,
                    order: 0.,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let sections = sections(&channels, &sidebar, &HashSet::new());
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["starred", "group:g", "channels", "dms"]);
        assert_eq!(sections[0].rows[0].id, "b");
    }
}
