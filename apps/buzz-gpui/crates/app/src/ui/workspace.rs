//! The window: header, community rail and the joined panel holding the
//! sidebar and the current page (ui.md §2).

mod board;
mod companion;
mod dialogs;
pub mod layout;
mod routes;
mod shell;

use super::account::Account;
use super::channel::{ChannelClosed, ChannelPage, Reveal};
use super::channel_states::{self, Failed, Reason};
use super::channel_tabs::{Routed, TabState};
use super::navigation::{Beside, ChannelRoute, History, LinkOrigin, Place, Request, Target};
use super::onboarding::{Onboarding, Step};
use super::pages::{self, Page};
use super::settings::{
    CloseSettings, OpenHostSettings, OpenSettings, Section, SectionChanged, SettingsPage,
};
use super::sidebar::SidebarView;
use super::thread;
use crate::community::{AppState, Community, IdentityState};
use crate::deep_links::{self, Link};
use crate::notifications::Conversation;
use buzz_relay_client::{ChannelSummary, RosterStatus};
use buzz_ui::{ActiveTheme, ShellBackground, TypeStyled};
use gpui_kit::component::window_border;
use gpui_kit::*;
use nostr::prelude::EventId;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

thread_local! {
    /// The current history entry's id and whether its opening control kept
    /// the focus, for views built while the workspace is busy.
    static VISIT: std::cell::Cell<(u64, bool)> = const { std::cell::Cell::new((0, false)) };
}

/// The current visit, as `(entry id, keeps focus)` (`PageNavigation`'s
/// `entryId` and `focus`); readable while the workspace itself updates.
pub fn current_visit() -> (u64, bool) {
    VISIT.get()
}

pub struct Workspace {
    page: Page,
    /// The header toggle's choice on a wide window.
    sidebar_open: bool,
    sidebar_motion: Option<shell::Motion>,
    /// On a narrow window the sidebar opens as a drawer over the page.
    drawer_open: bool,
    /// The current community's saved sidebar view (its width).
    sidebar_view: SidebarView,
    /// The header pills fold into More pages when they don't fit.
    compact_pages: bool,
    header_sizes: Rc<Cell<shell::HeaderSizes>>,
    overflow_open: bool,
    toggle_focus: FocusHandle,
    /// The page region, focused after a page change.
    main_focus: FocusHandle,
    /// A topbar tab is opening its page, whose content keeps from taking
    /// the focus.
    preserve_focus: bool,
    /// A plugin opens a page and keeps the focus where it is
    /// (`focus: "preserve"`).
    keep_focus: bool,
    /// Each channel's tab pane, per community, kept across visits.
    tab_states: HashMap<(EntityId, String), TabState>,
    /// The open channel's tab views, made on this visit.
    tab_views: HashMap<String, crate::ui::channel_tab_views::TabView>,
    tab_strip: ScrollHandle,
    /// The main pane's tab strip.
    main_strip: ScrollHandle,
    /// The main tab last scrolled into view.
    revealed_main: Option<String>,
    /// The channel and the main tab last shown, to tell a new selection.
    main_shown: Option<(Option<String>, Option<String>)>,
    /// A thread pushing in over the main tab it replaces, since when.
    main_motion: Option<(std::time::Instant, Option<String>)>,
    /// The next main selection comes from the keyboard, which skips the
    /// push.
    keyboard_switch: bool,
    /// Where tabs were laid out, for dragging them.
    tab_geometry: super::tab_drag::Geometry,
    /// Where a tab being dragged would land.
    drag_target: Option<super::tab_drag::DropTarget>,
    /// Escape or a resize cancelled the drag in progress.
    drag_cancelled: bool,
    /// A channel panel a header launcher opened, for this visit.
    drawer: Option<board::Drawer>,
    /// The tab last scrolled into view.
    revealed_tab: Option<String>,
    /// The tab pane's width once dragged, kept while it is closed.
    board_width: Option<f32>,
    pane_opening: u64,
    companion: Option<companion::Companion>,
    companion_width: Option<f32>,
    companion_opening: u64,
    community: Option<Entity<Community>>,
    channel: Option<Entity<ChannelPage>>,
    /// The thread the current visit addresses, with the root its route
    /// hints.
    routed: Option<Routed>,
    /// The message and thread the current conversation place names, as
    /// history records it.
    place: (Option<EventId>, Option<EventId>),
    /// The main pane, which owns Close while focus is in it.
    main_area: FocusHandle,
    /// The side pane.
    side_area: FocusHandle,
    /// Registered pages' views, made on first visit and kept.
    views: HashMap<Page, AnyView>,
    /// Pages' own sidebars, such as Me's.
    own_sidebars: HashMap<Page, AnyView>,
    settings: Option<Entity<SettingsPage>>,
    account: Entity<Account>,
    onboarding: Option<(Step, Entity<Onboarding>)>,
    _community_observer: Option<Subscription>,
    /// Every community's unread marks, for the rail.
    rail_marks: Entity<super::rail_menu::RailMarks>,
    /// Places visited, for Back and Forward.
    history: History,
    /// A place waiting for its community's channel list.
    pending: Option<Target>,
    /// The pending place is a conversation just opened, so it waits for
    /// the channel to be listed even once the roster has loaded.
    pending_listing: bool,
    /// Waits for the channel's first page to reveal a message in it.
    _revealing: Option<Subscription>,
    /// A routed channel the viewer has not joined, being looked up.
    resolving: Option<(Target, Task<()>)>,
    /// A place that could not open, kept for Retry navigation.
    failure: Option<Failed>,
    /// A settings card whose check is still running, waited for before
    /// Settings opens on it.
    checking: Option<Task<()>>,
    /// When the current attempt began: waiting and presenting share one
    /// open timeout (`controller.ts` `start()`).
    attempt_started: std::time::Instant,
    /// Whether the current attempt leaves the focus where it is
    /// (`focus: "preserve"`), for a presentation deferred or retried.
    attempt_keeps_focus: bool,
    /// The attempt that already checked a failed settings card again.
    rechecked: Option<u64>,
    dialog: Option<dialogs::OpenDialog>,
    _page_events: Option<Subscription>,
    /// Messages shows a route rather than a conversation.
    route: Option<ChannelRoute>,
    /// The New message page, while Messages shows that route.
    new_message: Option<Entity<super::new_message::NewMessagePage>>,
    /// The route a routed page shows (`Place::Route`).
    page_route: Option<serde_json::Value>,
    /// Bumped by every navigation, so a stale open timeout does nothing.
    attempts: u64,
    /// The page the current visit waits on to say it opened or failed
    /// (`handlesNavigation`).
    settling: Option<Page>,
    /// A link that arrived before an identity or a community was ready,
    /// with the viewer it arrived for (`deep-links.ts`).
    held: Option<Held>,
    /// The last place outside Settings, where its Back row returns.
    outside_settings: Option<Target>,
    _settings_events: Option<Subscription>,
}

/// A `buzz://` link waiting for an identity or a community.
#[derive(Clone)]
struct Held {
    url: String,
    viewer: Option<String>,
    /// The community selected when it first could be bound to one.
    community: Option<String>,
    /// It came from outside the window.
    ingress: bool,
    /// It was reported unavailable, which leaves it to Retry navigation.
    reported: bool,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_global_in::<AppState>(window, |this, window, cx| this.sync(window, cx))
            .detach();
        let rail_marks = cx.new(super::rail_menu::RailMarks::new);
        cx.observe(&rail_marks, |_, _, cx| cx.notify()).detach();
        // Notifications follow the conversation in front of the user.
        cx.observe_self(|this, cx| this.report_open(cx.active_window().is_some(), cx))
            .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            this.report_open(window.is_window_active(), cx)
        })
        .detach();
        // A page whose plugin was turned off leaves for Messages.
        cx.observe_global_in::<crate::plugins::Plugins>(window, |this, window, cx| {
            if this.page != Page::SETTINGS
                && this.page != Page::MESSAGES
                && pages::spec(this.page, cx).is_none()
            {
                this.select_page(Page::MESSAGES, window, cx);
            }
            cx.notify();
        })
        .detach();
        let mut workspace = Self {
            page: Page::MESSAGES,
            sidebar_open: true,
            sidebar_motion: None,
            drawer_open: false,
            sidebar_view: SidebarView::default(),
            compact_pages: layout::narrow(f32::from(window.viewport_size().width)),
            header_sizes: shell::header_sizes(),
            overflow_open: false,
            toggle_focus: cx.focus_handle(),
            main_focus: cx.focus_handle(),
            preserve_focus: false,
            keep_focus: false,
            tab_states: HashMap::new(),
            tab_views: HashMap::new(),
            tab_strip: ScrollHandle::new(),
            main_strip: ScrollHandle::new(),
            revealed_main: None,
            main_shown: None,
            main_motion: None,
            keyboard_switch: false,
            tab_geometry: Default::default(),
            drag_target: None,
            drag_cancelled: false,
            drawer: None,
            revealed_tab: None,
            board_width: None,
            pane_opening: 0,
            companion: None,
            companion_width: None,
            companion_opening: 0,
            community: None,
            channel: None,
            routed: None,
            place: (None, None),
            main_area: cx.focus_handle(),
            side_area: cx.focus_handle(),
            views: HashMap::new(),
            own_sidebars: HashMap::new(),
            settings: None,
            account: cx.new(Account::new),
            onboarding: None,
            _community_observer: None,
            rail_marks,
            history: History::default(),
            pending: None,
            pending_listing: false,
            _revealing: None,
            resolving: None,
            failure: None,
            checking: None,
            attempt_started: std::time::Instant::now(),
            attempt_keeps_focus: false,
            rechecked: None,
            dialog: None,
            _page_events: None,
            route: None,
            new_message: None,
            page_route: None,
            attempts: 0,
            settling: None,
            held: None,
            outside_settings: None,
            _settings_events: None,
        };
        workspace.handle_settings_actions(window, cx);
        workspace.sync(window, cx);
        workspace
    }

    /// Settings actions come from menus, other pages and Mod+, (bound in
    /// `main`), whatever has focus, so they are handled app-wide. They are
    /// dispatched while the window is borrowed, so the work is deferred.
    fn handle_settings_actions(&self, window: &Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        let weak = cx.entity().downgrade();
        let run = move |cx: &mut App, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let weak = weak.clone();
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| f(this, window, cx));
                });
            });
        };
        let (open_host, open_updates, close) = (run.clone(), run.clone(), run.clone());
        let app: &mut App = cx;
        app.on_action(move |_: &OpenSettings, cx| {
            run(cx, |this, window, cx| this.open_settings(None, window, cx))
        });
        app.on_action(move |_: &OpenHostSettings, cx| {
            open_host(cx, |this, window, cx| {
                this.open_settings(Some(Section::Agents), window, cx)
            })
        });
        app.on_action(move |_: &crate::updates::OpenUpdateSettings, cx| {
            open_updates(cx, |this, window, cx| {
                this.open_settings(Some(Section::Updates), window, cx)
            })
        });
        // Settings' Back row opens the last place outside Settings as a
        // new visit, or Messages (`leaveSettings`).
        app.on_action(move |_: &CloseSettings, cx| {
            close(cx, |this, window, cx| match this.outside_settings.clone() {
                Some(target) => this.navigate(target, window, cx),
                None => this.go_home(window, cx),
            })
        });
    }

    /// The Settings page, made on demand (history can return to it after it
    /// closed); its section changes are visits.
    fn settings_page(
        &mut self,
        section: Option<Section>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SettingsPage> {
        if let Some(settings) = &self.settings {
            if let Some(section) = section {
                settings.update(cx, |settings, cx| settings.open(section, cx));
            }
            return settings.clone();
        }
        let settings = cx.new(|cx| SettingsPage::new(section, window, cx));
        self._settings_events = Some(cx.subscribe(&settings, |this, _, _: &SectionChanged, cx| {
            if this.page == Page::SETTINGS {
                this.record(cx);
            }
        }));
        self.settings = Some(settings.clone());
        settings
    }

    fn open_settings(
        &mut self,
        section: Option<Section>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Without a community Settings still opens, as buzz-app's Personal
        // space allows; without an identity it does not.
        if matches!(self.onboarding, Some((Step::Identity, _))) {
            return;
        }
        self.settings_page(section, window, cx);
        if self.page != Page::SETTINGS {
            self.select_page(Page::SETTINGS, window, cx);
        }
    }

    /// Follows the selected community and the onboarding state.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx.global::<AppState>();
        // Signed in with no community is the personal space, not
        // onboarding: Settings and the agents work without one.
        let step = match &state.identity {
            IdentityState::Ready(_) => None,
            _ => Some(Step::Identity),
        };
        let viewer = match &state.identity {
            IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
            _ => None,
        };
        if self.onboarding.as_ref().map(|(s, _)| *s) != step {
            self.onboarding =
                step.map(|step| (step, cx.new(|cx| Onboarding::new(step, window, cx))));
            // Signed out: start over on Messages after the next sign-in.
            if step == Some(Step::Identity) {
                self.page = Page::MESSAGES;
                self.settings = None;
            }
        }
        let current = AppState::current(cx);
        if current.as_ref().map(Entity::entity_id) != self.community.as_ref().map(Entity::entity_id)
        {
            self.channel = None;
            self.leave_channel_tabs();
            self.companion = None;
            self.new_message = None;
            // The shown community's custom emoji load once it is shown
            // (`ChannelsPage` `emoji.ensure`).
            if let Some(community) = &current {
                community.read(cx).session.ensure_emoji();
            }
            self._community_observer = current.as_ref().map(|community| {
                cx.observe_in(community, window, |this, _, window, cx| {
                    this.restore_selection(window, cx);
                    cx.notify();
                })
            });
            self.community = current;
            self.load_sidebar_view(cx);
        }
        // A link held for an identity, or bound to the community first
        // selected, is denied once another one is (`flush`); an
        // invitation binds to no community.
        let selected = self
            .community
            .as_ref()
            .map(|community| community.read(cx).origin.to_string());
        let denied = self.held.as_mut().is_some_and(|held| {
            let invite = matches!(deep_links::parse(&held.url), Some(Link::Join { .. }));
            if !invite && held.community.is_none() {
                held.community = selected.clone();
            }
            (held.viewer.is_some() && held.viewer != viewer)
                || (!invite && held.community.is_some() && held.community != selected)
        });
        if denied {
            let ingress = self.held.take().is_some_and(|held| held.ingress);
            if ingress {
                self.begin_attempt();
                self.publish_visit();
                self.failure = Some(Failed {
                    target: None,
                    reason: Reason::Denied,
                    ingress,
                });
            }
        }
        // One from outside is asked again once the identity is read,
        // which reports it unavailable without a community.
        if viewer.is_some()
            && let Some(held) = &self.held
            && !held.reported
            && (self.community.is_some()
                || held.ingress
                || matches!(deep_links::parse(&held.url), Some(Link::Join { .. })))
        {
            let url = held.url.clone();
            self.held = None;
            let ingress = true;
            self.open_link(&url, ingress, window, cx);
        }
        cx.notify();
    }

    /// Reopens the channel last selected in this community once it is
    /// known to the session.
    fn restore_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.apply_pending(window, cx) {
            return;
        }
        // A route (New message, no conversation) is a place of its own.
        if self.channel.is_some()
            || self.route.is_some()
            || self.page != Page::MESSAGES
            || self.failure.is_some()
            || self.resolving.is_some()
        {
            return;
        }
        let Some(community) = &self.community else {
            return;
        };
        let remembered = community.read(cx).remembered_channel(cx);
        let snapshot = community.read(cx).snapshot.clone();
        let channel = remembered
            .and_then(|id| snapshot.channels.iter().find(|channel| channel.id == id))
            .cloned()
            .or_else(|| super::channel_states::default_channel(&snapshot));
        if let Some(channel) = channel {
            self.select_channel(&channel, window, cx);
        }
    }

    /// Opens a channel and records the visit, which shows its
    /// conversation even when it is already open.
    pub fn select_channel(
        &mut self,
        channel: &ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_channel(channel, window, cx);
        self.routed = None;
        self.place = (None, None);
        self.record(cx);
    }

    fn show_channel(
        &mut self,
        channel: &ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.page = Page::MESSAGES;
        self.route = None;
        self.failure = None;
        self.resolving = None;
        if self
            .channel
            .as_ref()
            .is_some_and(|page| page.read(cx).channel_id() == channel.id)
        {
            cx.notify();
            return;
        }
        let Some(community) = self.community.clone() else {
            return;
        };
        community.read(cx).remember_channel(&channel.id, cx);
        // Media hook: the old conversation's clips and viewer stop with it.
        super::media::pause_audio(None, cx);
        super::media::dismiss_review(window, cx);
        self.retire_menu_tabs(cx);
        self.leave_channel_tabs();
        self._revealing = None;
        let weak = cx.entity().downgrade();
        let open_thread: super::channel::OpenThread = Arc::new(move |row, reply, window, cx| {
            let root = row.thread_root_id.unwrap_or(row.id);
            let _ = weak.update(cx, |this, cx| {
                crate::perf::begin(crate::perf::THREAD);
                // Reply writes in the thread; anything else reads it.
                this.open_thread(row.id, root, reply, window, cx);
            });
        });
        let page = cx.new(|cx| {
            let mut page = ChannelPage::new(community, channel.clone(), open_thread, window, cx);
            page.live_status = true;
            // A session shows flat: it opens no threads.
            if channel.channel_type == Some(buzz_relay_client::ChannelType::Session) {
                page.without_threads(cx);
            }
            page
        });
        if !self.preserve_focus && !self.keep_focus {
            let composer = page.read(cx).composer();
            composer.update(cx, |state, cx| state.focus(window, cx));
        }
        self._page_events = Some(cx.subscribe_in(
            &page,
            window,
            |this, _, _: &ChannelClosed, window, cx| this.close_channel(window, cx),
        ));
        // "View channel details" opens the Channel settings tab.
        let weak = cx.entity().downgrade();
        let members = weak.clone();
        let actions = page.read(cx).header_actions();
        actions.update(cx, |actions, _| {
            actions.set_open_settings(Rc::new(move |_, window, cx| {
                let _ = weak.update(cx, |this, cx| this.open_channel_settings(window, cx));
            }));
            // Opening or closing members is a visit (`panel: "members"`).
            actions.set_on_members(Rc::new(move |cx| {
                let _ = members.update(cx, |this, cx| this.record(cx));
            }));
        });
        self.channel = Some(page);
        if let Some(root) = thread::dev_open_thread() {
            self.open_thread(root, root, false, window, cx);
        }
        if let Some(link) = dev_open_link() {
            self.route_link_to_tab(&link, window, cx);
        }
        cx.notify();
    }

    /// Shows a page and records the visit.
    pub fn select_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        // A page that completes its own visits is a visit it settles.
        if pages::settles(page, cx) {
            let community = self
                .community
                .as_ref()
                .map(|community| community.read(cx).origin.to_string());
            self.navigate(
                Target {
                    community,
                    place: Place::Page(page),
                },
                window,
                cx,
            );
            return;
        }
        // The page already shown, picked again, is a new attempt.
        if self.page == page {
            self.begin_attempt();
        }
        self.show_page(page, window, cx);
        // The visit is recorded first, so the page hears its entry.
        self.record(cx);
        self.open_page_route(page, None, window, cx);
    }

    /// Hands a routed page its route, or `None` for its start.
    fn open_page_route(
        &mut self,
        page: Page,
        params: Option<serde_json::Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.views.get(&page).cloned() else {
            return;
        };
        let Some(routes) = pages::routes(page, cx) else {
            // A page that settles its visits hears each one, routes or
            // not (a new `navigation` each attempt).
            if pages::settles(page, cx) {
                crate::external_plugins::ExternalPlugins::page_attempt(&view, window, cx);
            }
            return;
        };
        (routes.open)(&view, params.as_ref(), window, cx);
        self.page_route = params;
    }

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        // Media hook: clips stop while another page hides the conversation.
        if page != Page::MESSAGES {
            super::media::pause_audio(None, cx);
        }
        self.page = page;
        self.route = None;
        self.page_route = None;
        self.failure = None;
        self.resolving = None;
        if let Some(spec) = pages::spec(page, cx) {
            if let Some(build) = spec.build
                && !self.views.contains_key(&page)
            {
                let view = build(window, cx);
                self.views.insert(page, view);
            }
            if let pages::Navigation::Own(build) = spec.navigation
                && !self.own_sidebars.contains_key(&page)
            {
                let view = build(window, cx);
                self.own_sidebars.insert(page, view);
            }
        }
        if page != Page::SETTINGS {
            self.settings = None;
        }
        if page != Page::MESSAGES
            && let Some(community) = &self.community
        {
            community.read(cx).session.select(None);
        }
        // Focus moves to the page, as buzz-app's `#main-content` does; the
        // channel page focuses its composer instead, unless a topbar tab
        // opened it.
        if !self.keep_focus
            && (page != Page::MESSAGES || self.channel.is_none() || self.preserve_focus)
        {
            window.focus(&self.main_focus, cx);
        }
        cx.notify();
    }

    fn rail(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::rail_menu::rail(self.rail_marks.clone(), window, cx)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        self.ensure_page_view(window, cx);
        let dialog = self.dialog(window, cx);
        let main: AnyElement = if let Some((_, onboarding)) = &self.onboarding
            && self.page != Page::SETTINGS
        {
            onboarding.clone().into_any_element()
        } else if let Some(failed) = &self.failure {
            channel_states::failure(
                failed,
                cx.listener(|this, _, window, cx| this.retry_navigation(true, window, cx)),
                cx.listener(|this, _, window, cx| {
                    this.failure = None;
                    this.open_settings(None, window, cx)
                }),
                cx,
            )
        } else {
            match self.page {
                Page::SETTINGS => self.settings_page(None, window, cx).into_any_element(),
                Page::MESSAGES => match &self.channel {
                    Some(_) => self.board(window, cx),
                    None if self.route == Some(ChannelRoute::NewMessage)
                        && self.new_message.is_some() =>
                    {
                        self.new_message
                            .clone()
                            .expect("checked")
                            .into_any_element()
                    }
                    None => {
                        let snapshot = self
                            .community
                            .as_ref()
                            .map(|community| community.read(cx).snapshot.clone());
                        channel_states::render(
                            channel_states::empty(
                                snapshot.as_deref(),
                                self.pending.is_some(),
                                self.resolving.is_some(),
                            ),
                            self.route_placeholder(),
                            cx,
                        )
                    }
                },
                page => self.page_view(page),
            }
        };
        // Messages stacks the companion under its tab pane; other pages
        // get the shell's dock.
        let main = match self.page == Page::MESSAGES && self.channel.is_some() {
            true => main,
            false => self.frame(main, window, cx),
        };
        let header = self.header(window, cx);
        let body = self.body(main, window, cx);
        let gutter = layout::gutter(f32::from(window.viewport_size().width));
        let live_notice = self
            .community
            .as_ref()
            .filter(|_| self.onboarding.is_none())
            .and_then(|community| super::live_status::toast(community, window, cx));
        // No shadow around the frame: that needs a translucent surface,
        // which not every GPU path gives; edges resize from just inside.
        window_border().shadow_size(px(0.)).child(
            ShellBackground::new().child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .capture_key_down(|event, window, cx| {
                        if crate::shortcuts::external_key_down(event, window, cx) {
                            cx.stop_propagation();
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Navigate(NavigationDirection::Back),
                        cx.listener(|this, _, window, cx| this.go_back(window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Navigate(NavigationDirection::Forward),
                        cx.listener(|this, _, window, cx| this.go_forward(window, cx)),
                    )
                    .font_family("Inter")
                    .emoji_fallback()
                    .text_color(c.text_standard)
                    .child(header)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.rail(window, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .pr(px(gutter))
                                    .pb(px(gutter))
                                    .child(body),
                            ),
                    )
                    .children(dialog)
                    .children(crate::external_plugins::dialog(window, cx))
                    .children(live_notice)
                    .children(super::media::review_overlay(cx))
                    .children(super::toasts::render(window, cx)),
            ),
        )
    }
}

// Navigation: history, links and requests from outside the window.
impl Workspace {
    /// What the launch surface waits for (`data-buzz-launch-pending`):
    /// the channel list, a linked channel being looked up and the open
    /// channel's first page. Onboarding, other pages, a failed destination
    /// and a failed connection show at once.
    pub fn launch_content(&self, cx: &App) -> super::launch::Content {
        use super::launch::Content;
        use buzz_relay_client::SessionStatus;
        if self.onboarding.is_some() || self.page != Page::MESSAGES || self.failure.is_some() {
            return Content::Ready;
        }
        if self.resolving.is_some() {
            return Content::Required;
        }
        let Some(community) = &self.community else {
            return Content::Ready;
        };
        let snapshot = &community.read(cx).snapshot;
        if matches!(snapshot.status, SessionStatus::Failed(_)) {
            return Content::Ready;
        }
        if snapshot.roster == RosterStatus::Loading {
            return Content::Required;
        }
        match &self.channel {
            Some(channel) if channel.read(cx).first_page_loading(cx) => Content::Required,
            // Live updates are still connecting under a usable view.
            _ if !snapshot.live => Content::Settling,
            _ => Content::Ready,
        }
    }

    /// Where the window is now, as history records it.
    fn here(&self, cx: &App) -> Option<Target> {
        if self.onboarding.is_some() {
            return None;
        }
        let community = self
            .community
            .as_ref()
            .map(|community| community.read(cx).origin.to_string());
        if self.page == Page::MESSAGES
            && self.channel.is_none()
            && let Some(route) = &self.route
        {
            return Some(Target {
                community,
                place: Place::Channels(route.clone()),
            });
        }
        let place = match (self.page, &self.channel) {
            (Page::MESSAGES, Some(channel)) => {
                let channel = channel.read(cx).channel_id().to_string();
                let (message, thread) = self.place;
                Place::Conversation {
                    channel,
                    message,
                    thread,
                    members: self.members_open(cx),
                }
            }
            (Page::SETTINGS, _) => match &self.settings {
                Some(settings) => Place::Settings(settings.read(cx).section()),
                None => Place::Page(Page::SETTINGS),
            },
            (page, _) => match &self.page_route {
                Some(params) => Place::Route {
                    page,
                    params: params.clone(),
                    version: crate::ui::pages::routes(page, cx).map(|routes| routes.version),
                },
                None => Place::Page(page),
            },
        };
        Some(Target { community, place })
    }

    /// The conversation on screen: the open channel, and its thread while
    /// the tab pane shows it.
    fn on_screen(&self, cx: &App) -> Option<Conversation> {
        if self.onboarding.is_some() || self.page != Page::MESSAGES {
            return None;
        }
        let community = self.community.as_ref()?.read(cx).origin.to_string();
        let channel = self.channel.as_ref()?.read(cx).channel_id().to_string();
        let thread = self
            .shown_thread(cx)
            .filter(|(of, _)| *of == channel)
            .map(|(_, root)| root);
        Some(Conversation {
            community,
            channel,
            thread,
        })
    }

    /// Tells notifications what is on screen, which reads its banners
    /// while the window is `active`.
    fn report_open(&self, active: bool, cx: &mut App) {
        let open = self.on_screen(cx);
        crate::notifications::showing(open, active, cx);
    }

    /// A registered page's view. Every bundled page uses buzz-app's
    /// workspace layout and owns its padding; document layout is only for
    /// external plugins, which are skipped.
    /// A page registered again after it was taken away is built anew
    /// when shown, as it remounts.
    /// A failed visit mounts nothing until Retry, which builds it with its
    /// route.
    fn ensure_page_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.failure.is_some()
            || self.views.contains_key(&self.page)
            || !self.page_route_stands(self.page, cx)
        {
            return;
        }
        if let Some(build) = pages::spec(self.page, cx).and_then(|spec| spec.build) {
            let view = build(window, cx);
            // It opens at the visit's route, as it remounts with it.
            if let Some(route) = &self.page_route {
                crate::external_plugins::ExternalPlugins::page_resolved(&view, route, cx);
            }
            self.views.insert(self.page, view);
        }
    }

    /// The visit's route still suits `page`: it takes routes, at the
    /// version the route was made for, and these params. A page back at
    /// another version mounts nothing (`unavailable`).
    pub(crate) fn page_route_stands(&self, page: Page, cx: &App) -> bool {
        if page != self.page || self.page_route.is_none() {
            return true;
        }
        match self.history.current().map(|target| &target.place) {
            Some(Place::Route {
                params, version, ..
            }) => pages::routes(page, cx).is_some_and(|routes| {
                version.is_none_or(|version| version == routes.version) && (routes.validate)(params)
            }),
            _ => true,
        }
    }

    /// Closes the view of `page`, whose route no longer suits it.
    pub(crate) fn drop_page_view(&mut self, page: Page, cx: &mut Context<Self>) {
        self.views.remove(&page);
        cx.notify();
    }

    fn page_view(&self, page: Page) -> AnyElement {
        self.views
            .get(&page)
            .cloned()
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| div().into_any_element())
    }

    /// A new attempt replaces whatever the last one still had going
    /// (`start()` settles it `superseded`).
    fn begin_attempt(&mut self) {
        self.attempts += 1;
        self.attempt_started = std::time::Instant::now();
        self.attempt_keeps_focus = self.keep_focus || self.preserve_focus;
        self.settling = None;
        self.checking = None;
        self.pending = None;
        self.pending_listing = false;
        self.resolving = None;
        self._revealing = None;
        self.failure = None;
        // A newer visit drops the link left to Retry navigation.
        if self.held.as_ref().is_some_and(|held| held.reported) {
            self.held = None;
        }
    }

    /// What is left of the current attempt's open timeout.
    fn attempt_left(&self) -> std::time::Duration {
        channel_states::OPEN_TIMEOUT.saturating_sub(self.attempt_started.elapsed())
    }

    /// Records the place the user just went to.
    fn record(&mut self, cx: &mut Context<Self>) {
        // Any navigation closes the narrow drawer.
        self.drawer_open = false;
        if let Some(here) = self.here(cx) {
            // The first place recorded is the startup restore, not a visit.
            super::search_kit::record_visit(
                self.community.as_ref(),
                &here.place,
                self.history.current().is_none(),
                cx,
            );
            if self.history.push(here) {
                // A visit of the user's own supersedes one still opening.
                if self.visit().1 {
                    self.begin_attempt();
                } else {
                    // A visit of the user's own is an attempt of its own.
                    self.attempts += 1;
                    self.attempt_started = std::time::Instant::now();
                    self.attempt_keeps_focus = self.keep_focus || self.preserve_focus;
                }
                self.enter_visit(cx);
            }
        }
        self.publish_visit();
        cx.notify();
    }

    /// The current history entry and focus, for views built while the
    /// workspace updates.
    fn publish_visit(&self) {
        VISIT.set((
            self.history.current_id().unwrap_or_default(),
            self.keep_focus || self.preserve_focus,
        ));
    }

    fn remember_outside_settings(&mut self, target: &Target) {
        if !matches!(
            target.place,
            Place::Settings(_) | Place::Page(Page::SETTINGS)
        ) {
            self.outside_settings = Some(target.clone());
        }
    }

    /// The place history is at, and whether Back and Forward go anywhere.
    /// The current history entry's id: the same through Retry and Back or
    /// Forward to it (`PageNavigation.entryId`).
    pub(crate) fn entry(&self) -> Option<u64> {
        self.history.current_id()
    }

    pub(crate) fn location(&self) -> (Option<Target>, bool, bool) {
        (
            self.history.current().cloned(),
            self.history.can_go_back(),
            self.history.can_go_forward(),
        )
    }

    /// Why the last visit could not open, and whether retrying may help.
    pub(crate) fn failed(&self) -> Option<(&super::channel_states::Failed, bool)> {
        self.failure
            .as_ref()
            .map(|failed| (failed, failed.retryable()))
    }

    /// A plugin's `navigation/retry`: the visit again, the relay as it is
    /// (`controller.retry()` never reconnects).
    pub(crate) fn retry_visit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.retry_navigation(false, window, cx);
    }

    /// The visit being opened, by attempt, and whether it is still
    /// opening: its channel listed or looked up.
    /// Whether the current attempt keeps the focus where it is
    /// (`attempt.focus` `preserve`).
    pub(crate) fn attempt_keeps_focus(&self) -> bool {
        self.attempt_keeps_focus
    }

    pub(crate) fn visit(&self) -> (u64, bool) {
        (
            self.attempts,
            self.resolving.is_some()
                || self._revealing.is_some()
                || self.pending.is_some()
                || self.pending_listing
                || self.settling.is_some()
                || self.checking.is_some(),
        )
    }

    /// A navigation that could not start fails as a visit, as buzz-app
    /// records it, so the window and `navigation/get` say why.
    pub(crate) fn fail_open(
        &mut self,
        target: Option<Target>,
        reason: channel_states::Reason,
        cx: &mut Context<Self>,
    ) {
        self.attempts += 1;
        self.settling = None;
        self.checking = None;
        // What an earlier attempt still had going ends with it.
        self.pending = None;
        self.pending_listing = false;
        self.resolving = None;
        self._revealing = None;
        self.failure = Some(Failed {
            target,
            reason,
            ingress: false,
        });
        cx.notify();
    }

    /// The page showing the current visit says it opened
    /// (`PageNavigation.complete({status: "opened"})`); `false` when the
    /// visit does not wait on page `key`.
    pub(crate) fn settle_visit(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if self.settling.is_none_or(|page| page.key() != key) {
            return false;
        }
        self.settling = None;
        cx.notify();
        true
    }

    /// The page the current visit waits on picks its route itself
    /// (`PageNavigation.resolve`): the entry shows `params` without
    /// another visit. `false` when the visit does not wait on page `key`.
    pub(crate) fn resolve_visit(
        &mut self,
        key: &str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(page) = self.settling.filter(|page| page.key() == key) else {
            return false;
        };
        let Some(current) = self.history.current().cloned() else {
            return false;
        };
        let routes = crate::ui::pages::routes(page, cx);
        let target = Target {
            community: current.community,
            place: Place::Route {
                page,
                params: params.clone(),
                version: routes.map(|routes| routes.version),
            },
        };
        self.history.resolve(target.clone());
        // A route the page does not take fails the visit as `unavailable`.
        if routes.is_none_or(|routes| !(routes.validate)(&params)) {
            self.fail_presenting(target, Reason::Page, cx);
            return true;
        }
        // The page keeps it as its route, so a view made anew opens there.
        if let Some(view) = self.views.get(&page) {
            crate::external_plugins::ExternalPlugins::page_resolved(view, &params, cx);
        }
        self.page_route = Some(params);
        cx.notify();
        true
    }

    /// A page showing `page` moved to route `params` itself, as Me does
    /// when a conversation is picked: a new visit, already shown, so Back
    /// returns to the one before. Nothing while another page shows.
    pub(crate) fn record_route(
        &mut self,
        page: Page,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.page != page {
            return;
        }
        let community = self
            .community
            .as_ref()
            .map(|community| community.read(cx).origin.to_string());
        let version = pages::routes(page, cx).map(|routes| routes.version);
        let target = Target {
            community,
            place: Place::Route {
                page,
                params: params.clone(),
                version,
            },
        };
        if self.history.current() == Some(&target) {
            return;
        }
        self.history.push(target);
        self.begin_attempt();
        self.page_route = Some(params);
        self.publish_visit();
        cx.notify();
    }

    /// Fails a visit its page never settled within the open timeout.
    fn arm_settle_timeout(&mut self, cx: &mut Context<Self>) {
        let (attempt, left) = (self.attempts, self.attempt_left());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(left).await;
            let _ = this.update(cx, |this, cx| {
                if this.attempts == attempt && this.settling.take().is_some() {
                    this.failure = Some(Failed {
                        target: this.history.current().cloned(),
                        reason: Reason::Timeout,
                        ingress: false,
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The page the current visit waits on says it cannot
    /// (`PageNavigation.complete({status: "failed"})`): the visit fails,
    /// kept for Retry navigation. Only while the visit waits on page `key`
    /// (`handlesNavigation`); `false` otherwise.
    pub(crate) fn fail_visit(
        &mut self,
        key: &str,
        reason: super::channel_states::Reason,
        cx: &mut Context<Self>,
    ) -> bool {
        let target = self.history.current().cloned();
        // Only while the visit still waits on this page: a visit that
        // opened stays opened (`complete` refuses once it is not pending).
        if self.settling.is_none_or(|page| page.key() != key) {
            return false;
        }
        self.settling = None;
        self.failure = Some(super::channel_states::Failed {
            target,
            reason,
            ingress: false,
        });
        cx.notify();
        true
    }

    /// Opens `target` without moving the focus, for a plugin control
    /// that keeps it (`focus: "preserve"`).
    pub fn navigate_keeping_focus(
        &mut self,
        target: Target,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keep_focus = true;
        if replace {
            self.navigate_replacing(target, window, cx);
        } else {
            self.navigate(target, window, cx);
        }
        self.keep_focus = false;
    }

    /// Goes to `target` as a new visit.
    pub fn navigate(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_outside_settings(&target);
        self.history.push(target.clone());
        let place = target.place.clone();
        self.apply(target, window, cx);
        super::search_kit::record_visit(self.community.as_ref(), &place, false, cx);
    }

    /// Goes to `target` in place of the current visit (`replace: true`),
    /// as closing a routed dialog does.
    pub fn navigate_replacing(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_outside_settings(&target);
        // The place already shown is a new attempt at the same entry.
        if self.history.current() != Some(&target) {
            self.history.replace(target.clone());
        }
        self.apply(target, window, cx);
    }

    /// Selecting a community in the rail is a visit to its Messages page,
    /// except while an outside link waits to be retried there.
    pub(crate) fn select_community(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recovering = self
            .failure
            .as_ref()
            .is_some_and(|failed| failed.ingress && failed.retryable());
        let origin = cx
            .global::<AppState>()
            .communities
            .get(index)
            .map(|community| community.read(cx).origin.to_string());
        match origin {
            Some(origin) if !recovering => self.navigate(
                Target {
                    community: Some(origin),
                    place: Place::Page(Page::MESSAGES),
                },
                window,
                cx,
            ),
            _ => AppState::select(index, cx),
        }
    }

    /// Personal space: no community, on the Messages page, except while an
    /// outside link waits to be retried.
    pub(crate) fn select_personal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let recovering = self
            .failure
            .as_ref()
            .is_some_and(|failed| failed.ingress && failed.retryable());
        AppState::select_personal(cx);
        self.sync(window, cx);
        if !recovering {
            self.select_page(Page::MESSAGES, window, cx);
        }
    }

    /// "Retry navigation": the kept target again, reconnecting its
    /// community if the connection failed.
    fn retry_navigation(&mut self, repair: bool, window: &mut Window, cx: &mut Context<Self>) {
        // With nothing failed, the current visit starts over (`retry()`
        // always starts a new attempt).
        let Some(failed) = self.failure.take() else {
            if let Some(target) = self.history.current().cloned() {
                let keep = std::mem::replace(&mut self.keep_focus, self.attempt_keeps_focus);
                self.apply(target, window, cx);
                self.keep_focus = keep;
            }
            return;
        };
        if repair {
            self.retry_session(cx);
        }
        match failed.target {
            // With the failed attempt's focus choice (`retry()`).
            Some(target) => {
                let keep = std::mem::replace(&mut self.keep_focus, self.attempt_keeps_focus);
                self.apply(target, window, cx);
                self.keep_focus = keep;
            }
            // A held link is asked again (`openHeld`).
            None if failed.reason == Reason::Held
                && let Some(held) = self.held.take() =>
            {
                self.open_link(&held.url, true, window, cx)
            }
            // Nothing to open again: the visit fails as before, as a new
            // attempt (`retry`).
            None => self.fail_open(None, failed.reason, cx),
        }
    }

    /// Retries a failed community session, as Retry navigation does
    /// before it opens the place again.
    fn retry_session(&self, cx: &App) {
        if let Some(community) = &self.community {
            let community = community.read(cx);
            if matches!(
                community.snapshot.status,
                buzz_relay_client::SessionStatus::Failed(_)
            ) {
                community.session.retry();
            }
        }
    }

    /// Fails the place still waiting for its channel list, or still being
    /// looked up, after [`OPEN_TIMEOUT`] (`controller.ts`).
    ///
    /// [`OPEN_TIMEOUT`]: channel_states::OPEN_TIMEOUT
    fn arm_open_timeout(&mut self, ingress: bool, cx: &mut Context<Self>) {
        let (attempt, left) = (self.attempts, self.attempt_left());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(left).await;
            let _ = this.update(cx, |this, cx| {
                let waiting = match this.resolving.take() {
                    Some((target, _)) => Some(target),
                    None => this.pending.take(),
                };
                if this.attempts == attempt
                    && let Some(target) = waiting
                {
                    this.pending_listing = false;
                    this.failure = Some(Failed {
                        target: Some(target),
                        reason: Reason::Timeout,
                        ingress,
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.history.back() {
            self.apply(target, window, cx);
        }
    }

    pub fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.history.forward() {
            self.apply(target, window, cx);
        }
    }

    /// The Messages page, on the channel last open there.
    pub fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_page(Page::MESSAGES, window, cx);
        if self.channel.is_none() {
            self.restore_selection(window, cx);
        }
        self.record(cx);
    }

    /// Shows `target` without recording it.
    /// Waits, under the open timeout, while `waiting` holds (a settings
    /// card still checking, a plugin still starting), then presents
    /// `target` again if this visit is still the latest; one still
    /// waiting at the timeout fails as `timeout`.
    fn wait_then_present(
        &mut self,
        target: Target,
        waiting: fn(&Target, &App) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (attempt, deadline) = (
            self.attempts,
            std::time::Instant::now() + self.attempt_left(),
        );
        // Settings' checks run meanwhile, as they do while it shows.
        let held = super::settings::cards::retain_visibility(cx);
        self.checking = Some(cx.spawn_in(window, async move |this, cx| {
            let _held = held;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let still = cx.update(|_, cx| waiting(&target, cx)).unwrap_or(false);
                let timed_out = std::time::Instant::now() >= deadline;
                if still && !timed_out {
                    continue;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.attempts != attempt {
                        return;
                    }
                    this.checking = None;
                    if still {
                        this.fail_presenting(target, Reason::Timeout, cx);
                    } else {
                        // With the attempt's own focus choice.
                        let keep =
                            std::mem::replace(&mut this.keep_focus, this.attempt_keeps_focus);
                        this.present(target, window, cx);
                        this.keep_focus = keep;
                    }
                });
                return;
            }
        }));
        cx.notify();
    }

    /// The visit fails as it presents, kept for Retry navigation.
    pub(super) fn fail_presenting(
        &mut self,
        target: Target,
        reason: Reason,
        cx: &mut Context<Self>,
    ) {
        self.failure = Some(Failed {
            target: Some(target),
            reason,
            ingress: false,
        });
        self.settling = None;
        cx.notify();
    }

    fn apply(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.drawer_open = false;
        // Every visit closes a media review (`mediaReviewForDestination`).
        super::media::dismiss_review(window, cx);
        self.begin_attempt();
        self.publish_visit();
        self.settling = None;
        if let Some(origin) = &target.community {
            let state = cx.global::<AppState>();
            let index = state
                .communities
                .iter()
                .position(|community| community.read(cx).origin.to_string() == *origin);
            let Some(index) = index else {
                tracing::warn!(%origin, "that community is no longer saved");
                self.page = Page::MESSAGES;
                self.failure = Some(Failed {
                    target: Some(target.clone()),
                    reason: Reason::Denied,
                    ingress: false,
                });
                cx.notify();
                return;
            };
            if state.selected != Some(index) {
                AppState::select(index, cx);
                self.sync(window, cx);
            }
        } else if AppState::current(cx).is_some() {
            // A Personal-space visit leaves the community it was not in.
            AppState::select_personal(cx);
            self.sync(window, cx);
        }
        self.present(target, window, cx);
    }

    /// Whether `target`'s page or settings card waits on its plugin,
    /// still starting, or on its card's check.
    fn presenting_waits(target: &Target, cx: &App) -> bool {
        use super::settings::cards::{self, CardVisibility};
        let starting = |key: &str| {
            crate::external_plugins::ExternalPlugins::starting(crate::plugins::plugin_of(key), cx)
        };
        match &target.place {
            Place::Page(page) | Place::Route { page, .. } => {
                pages::spec(*page, cx).is_none() && starting(page.key())
            }
            // A community's section waits for one to be selected, as
            // buzz-app's Settings never shows it in Personal space.
            Place::Settings(super::settings::Section::CustomEmoji) => {
                AppState::current(cx).is_none()
            }
            Place::Settings(super::settings::Section::Card(key)) => {
                let personal = AppState::current(cx).is_none();
                match cards::get(key, cx) {
                    Some(card)
                        if personal
                            && !matches!(card.placement, cards::CardPlacement::Group(_)) =>
                    {
                        true
                    }
                    Some(card) => card.visibility(cx) == CardVisibility::Pending,
                    None => starting(key),
                }
            }
            Place::Unresolved(request) => {
                crate::external_plugins::ExternalPlugins::resolve_request(request, cx).is_none()
                    && ["page", "section"]
                        .iter()
                        .filter_map(|key| request.get(key).and_then(serde_json::Value::as_str))
                        .any(starting)
            }
            _ => false,
        }
    }

    /// Shows `target` for the current attempt once its place is there, as
    /// buzz-app's presentation checks on every attempt (Retry, Back and
    /// Forward too): a request kept for a place Buzz lacked resolves anew;
    /// a page, route or settings card that is gone fails as
    /// `unavailable`; a plugin still starting, or a card still checking,
    /// is waited for, and a card whose check failed is checked again.
    fn present(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        use super::settings::cards::{self, CardVisibility};
        if Self::presenting_waits(&target, cx) {
            self.wait_then_present(target, Self::presenting_waits, window, cx);
            return;
        }
        let missing = match &target.place {
            Place::Unresolved(request) => {
                match crate::external_plugins::ExternalPlugins::resolve_request(request, cx) {
                    Some(place) => {
                        let resolved = Target {
                            community: target.community.clone(),
                            place,
                        };
                        return self.present(resolved, window, cx);
                    }
                    None => true,
                }
            }
            Place::Page(page) => *page != Page::SETTINGS && pages::spec(*page, cx).is_none(),
            Place::Route {
                page,
                params,
                version,
            } => {
                pages::spec(*page, cx).is_none()
                    || pages::routes(*page, cx).is_none_or(|routes| {
                        version.is_some_and(|version| version != routes.version)
                            || !(routes.validate)(params)
                    })
            }
            Place::Settings(super::settings::Section::Card(key)) => {
                match cards::get(key, cx).map(|card| card.visibility(cx)) {
                    Some(CardVisibility::Visible) => false,
                    // Checked again once per attempt; failing again, it is
                    // unavailable.
                    // A hidden or failed plugin card is checked once per
                    // attempt (`retainVisibility` ensures it) before the
                    // visit gives up on it.
                    Some(CardVisibility::Error | CardVisibility::Hidden)
                        if self.rechecked != Some(self.attempts)
                            && crate::plugins::enabled_key(key)
                            && crate::external_plugins::ExternalPlugins::recheck_card(key, cx) =>
                    {
                        self.rechecked = Some(self.attempts);
                        self.wait_then_present(target, Self::presenting_waits, window, cx);
                        return;
                    }
                    _ => true,
                }
            }
            _ => false,
        };
        if missing {
            self.fail_presenting(target, Reason::Page, cx);
            return;
        }
        self.settling = match &target.place {
            Place::Page(page) | Place::Route { page, .. } if pages::settles(*page, cx) => {
                self.arm_settle_timeout(cx);
                Some(*page)
            }
            _ => None,
        };
        match target.place {
            Place::Channels(route) => self.show_route(route, window, cx),
            Place::Page(page) => {
                self.show_page(page, window, cx);
                self.open_page_route(page, None, window, cx);
                if page == Page::MESSAGES && self.channel.is_none() {
                    self.restore_selection(window, cx);
                }
            }
            Place::Route {
                page, ref params, ..
            } => {
                self.show_page(page, window, cx);
                self.open_page_route(page, Some(params.clone()), window, cx);
            }
            // Resolved above.
            Place::Unresolved(_) => {}
            Place::Settings(section) => {
                if matches!(self.onboarding, Some((Step::Identity, _))) {
                    return;
                }
                self.settings_page(Some(section), window, cx);
                self.show_page(Page::SETTINGS, window, cx);
            }
            Place::Conversation {
                ref channel,
                message,
                thread,
                members,
            } => {
                let Some(community) = &self.community else {
                    return;
                };
                let snapshot = community.read(cx).snapshot.clone();
                // A preview already shown stays while a visit within it,
                // such as one of its threads, opens.
                let shown = self
                    .channel
                    .as_ref()
                    .map(|page| page.read(cx).channel().clone())
                    .filter(|shown| shown.id == *channel && shown.read_only && !shown.cached);
                let Some(summary) = snapshot
                    .channels
                    .iter()
                    .find(|c| c.id == *channel)
                    .or(shown.as_ref())
                else {
                    // A link can arrive before the channel list does; one
                    // the viewer has not joined is looked up exactly.
                    if snapshot.roster == RosterStatus::Ready {
                        self.resolve(target.clone(), window, cx);
                    } else {
                        self.page = Page::MESSAGES;
                        self.pending = Some(target.clone());
                        self.arm_open_timeout(false, cx);
                    }
                    cx.notify();
                    return;
                };
                self.open_place(summary, (message, thread, members), window, cx);
            }
        }
        cx.notify();
    }

    /// Shows a channel, then the message or thread the place names. Each
    /// history entry applies to the channel's tabs once: a thread visit
    /// selects its thread, another restores the tabs it had, and none is
    /// ever closed.
    fn open_place(
        &mut self,
        summary: &ChannelSummary,
        (message, thread, members): (Option<EventId>, Option<EventId>, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_channel(summary, window, cx);
        self.place = (message, thread);
        self.routed = None;
        // The members dialog first: a thread it opens over keeps it focused.
        if let Some(channel) = &self.channel {
            let actions = channel.read(cx).header_actions();
            actions.update(cx, |actions, cx| actions.show_members(members, window, cx));
        }
        match Beside::of(message, thread) {
            Beside::Thread(root) => self.route_thread(root, root, window, cx),
            // A message still finding its place waits to apply its visit.
            Beside::Find { message, thread } => self.reveal(message, thread, window, cx),
            Beside::Nothing => self.enter_visit(cx),
        }
    }

    /// The open channel's members dialog is showing.
    fn members_open(&self, cx: &App) -> bool {
        self.channel
            .as_ref()
            .is_some_and(|channel| channel.read(cx).header_actions().read(cx).members_open())
    }

    /// Looks up a routed channel the viewer has not joined
    /// ("Checking conversation access…"), opening its read-only preview,
    /// or the failure notice when it cannot be read.
    fn resolve(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(community), Place::Conversation { channel, .. }) =
            (self.community.clone(), &target.place)
        else {
            return;
        };
        self.page = Page::MESSAGES;
        self.channel = None;
        self.leave_channel_tabs();
        let session = community.read(cx).session.clone();
        let id = channel.clone();
        let task = cx.spawn_in(window, {
            let target = target.clone();
            async move |this, cx| {
                let found = session.resolve_channels(vec![id.clone()]).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.resolving = None;
                    let Place::Conversation {
                        message,
                        thread,
                        members,
                        ..
                    } = target.place
                    else {
                        return;
                    };
                    // A channel this window cannot show is unavailable
                    // (`ChannelsPage`), found or not.
                    let reason = Reason::Page;
                    match found
                        .ok()
                        .and_then(|list| list.into_iter().find(|c| c.id == id))
                    {
                        Some(summary) => {
                            this.open_place(&summary, (message, thread, members), window, cx)
                        }
                        None => {
                            tracing::warn!(channel = %id, "that channel is not available here");
                            this.failure = Some(Failed {
                                target: Some(target),
                                reason,
                                ingress: false,
                            });
                        }
                    }
                    cx.notify();
                });
            }
        });
        self.resolving = Some((target, task));
        self.arm_open_timeout(false, cx);
    }

    /// Applies a place that was waiting for its channel list. True while
    /// it still waits or once it opened, so the remembered channel does
    /// not replace it.
    fn apply_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.pending.clone() else {
            return false;
        };
        let Some(community) = &self.community else {
            return false;
        };
        let community = community.read(cx);
        // The user moved on to another community.
        if target.community.as_deref() != Some(&community.origin.to_string()) {
            self.pending = None;
            return false;
        }
        let snapshot = community.snapshot.clone();
        let Place::Conversation { channel, .. } = &target.place else {
            return false;
        };
        let known = snapshot.channels.iter().any(|c| c.id == *channel);
        if !known && (snapshot.roster != RosterStatus::Ready || self.pending_listing) {
            return true;
        }
        // Either opens it or gives up on it, keeping it for retry.
        self.apply(target, window, cx);
        known || self.failure.is_some()
    }

    /// Opens a conversation the session has just made or joined, once the
    /// community's channel list includes it.
    pub fn open_conversation(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = &self.community else {
            return;
        };
        let community = community.read(cx);
        let listed = community.snapshot.channels.iter().any(|c| c.id == id);
        let target = Target::channel(Some(community.origin.to_string()), id);
        if listed {
            self.navigate(target, window, cx);
        } else {
            self.begin_attempt();
            self.pending = Some(target);
            self.pending_listing = true;
            self.arm_open_timeout(false, cx);
        }
    }

    /// Reveals a message in the open channel: in the timeline when it is a
    /// loaded top-level message, else in its thread's tab, whose root
    /// `thread` hints (relay.md §5c.5).
    fn reveal(
        &mut self,
        message: EventId,
        thread: Option<EventId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.channel.clone() else {
            return;
        };
        let outcome = page.update(cx, |page, cx| page.reveal(message, cx));
        self.revealed(outcome, message, thread, window, cx);
        if outcome == Reveal::Loading {
            self._revealing = Some(cx.observe_in(&page, window, move |this, page, window, cx| {
                let outcome = page.update(cx, |page, cx| page.reveal(message, cx));
                this.revealed(outcome, message, thread, window, cx);
            }));
        }
    }

    fn revealed(
        &mut self,
        outcome: Reveal,
        message: EventId,
        thread: Option<EventId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Reveal::Loading => {}
            Reveal::Shown => {
                self._revealing = None;
                self.enter_visit(cx);
            }
            Reveal::Absent => {
                self._revealing = None;
                self.route_thread(message, thread.unwrap_or(message), window, cx);
            }
        }
    }

    /// The routed thread found its message, or found it missing: the visit
    /// opens, or fails as not found (unavailable on a read error).
    fn routed_status(&mut self, panel: &Entity<thread::ThreadPanel>, cx: &mut Context<Self>) {
        use buzz_relay_client::threads::TargetStatus;
        let panel = panel.read(cx);
        let reason = match panel.target_status() {
            Some(TargetStatus::Loading) => return,
            Some(TargetStatus::Ready) => None,
            Some(TargetStatus::Unavailable) => Some(Reason::NotFound),
            Some(TargetStatus::Error) => Some(Reason::Page),
            // Opened at its root: the visit opens once the root loads.
            None => match panel.root_found() {
                None => return,
                Some(true) => None,
                Some(false) => Some(Reason::NotFound),
            },
        };
        self._revealing = None;
        if let Some(reason) = reason
            && let Some(target) = self.history.current().cloned()
        {
            self.fail_presenting(target, reason, cx);
        }
        cx.notify();
    }

    /// The current visit addresses a thread: its tab is selected, or
    /// added, and shows `message`.
    fn route_thread(
        &mut self,
        message: EventId,
        root: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.as_ref() else {
            return;
        };
        self.routed = Some(Routed {
            channel: channel.read(cx).channel_id().to_string(),
            message,
            root,
        });
        self.enter_visit(cx);
        // The visit opens once the thread finds its message, and fails as
        // not found when it is missing (`ThreadPanel`'s exact target).
        if let Some(panel) = self.routed_thread_panel(window, cx) {
            self._revealing =
                Some(cx.observe(&panel, |this, panel, cx| this.routed_status(&panel, cx)));
            self.routed_status(&panel, cx);
        }
        // Routed openings move focus to the thread, unless a dialog, such
        // as the members one this visit opens, holds it.
        if self.dialog.is_none()
            && !self.members_open(cx)
            && let Some(id) = self.shown_thread_tab(cx)
        {
            self.focus_routed(&id, window, cx);
        }
    }

    /// A `buzz://` link: conversations bind to the selected community;
    /// invitations add theirs. Web links go to the browser.
    pub fn open_url(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.route_link_to_tab(url, window, cx) {
            return;
        }
        self.open_link(url, false, window, cx);
    }

    /// A web link clicked in the window: true when a channel panel took
    /// it as a tab beside the conversation.
    pub fn open_panel_link(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.route_link_to_tab(url, window, cx)
    }

    /// [`Self::open_panel_link`] from a message.
    pub fn open_panel_link_from(
        &mut self,
        url: &str,
        origin: Option<super::navigation::LinkOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.route_link_from(url, origin, window, cx)
    }

    /// A link from outside the window, or one no tab takes. With
    /// `ingress` (from outside) an invalid link says so instead of being
    /// ignored.
    fn open_link(&mut self, url: &str, ingress: bool, window: &mut Window, cx: &mut Context<Self>) {
        let viewer = match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
            _ => None,
        };
        match deep_links::parse(url) {
            // Held until there is an identity, and for a conversation a
            // community to bind it to.
            Some(Link::Join { .. }) if viewer.is_none() => {
                self.held = Some(Held {
                    url: url.to_string(),
                    viewer: None,
                    community: None,
                    ingress,
                    reported: false,
                });
            }
            // Until there is an identity it waits; then, without a
            // community, it is unavailable, kept for Retry navigation
            // (`deep-links.ts`).
            Some(Link::Conversation { .. } | Link::Entity(_))
                if viewer.is_none() || self.community.is_none() =>
            {
                let mut held = Held {
                    url: url.to_string(),
                    viewer: viewer.clone(),
                    community: None,
                    ingress,
                    reported: false,
                };
                if viewer.is_none() || !ingress {
                    self.held = Some(held);
                    return;
                }
                self.begin_attempt();
                self.publish_visit();
                self.failure = Some(Failed {
                    target: None,
                    reason: Reason::Held,
                    ingress,
                });
                held.reported = true;
                self.held = Some(held);
                self.page = Page::MESSAGES;
                cx.notify();
            }
            // Entity links open the Projects page in the selected community.
            Some(Link::Entity(route)) => {
                let community = self
                    .community
                    .as_ref()
                    .map(|community| community.read(cx).origin.to_string());
                let target = Target {
                    community,
                    place: Place::Route {
                        page: super::projects::PAGE,
                        params: route.params(),
                        version: crate::ui::pages::routes(super::projects::PAGE, cx)
                            .map(|routes| routes.version),
                    },
                };
                self.navigate(target, window, cx);
            }
            Some(Link::Conversation {
                channel,
                message,
                thread,
            }) => {
                let Some(community) = &self.community else {
                    return;
                };
                let community = Some(community.read(cx).origin.to_string());
                let target = Target {
                    community,
                    place: Place::Conversation {
                        channel,
                        message,
                        thread,
                        members: false,
                    },
                };
                self.navigate(target, window, cx);
            }
            Some(Link::Join { .. }) => self.open_invite(url, window, cx),
            None if url.starts_with("https://") || url.starts_with("http://") => cx.open_url(url),
            None if ingress => {
                self.begin_attempt();
                self.publish_visit();
                self.failure = Some(Failed {
                    target: None,
                    reason: Reason::Invalid,
                    ingress,
                });
                self.page = Page::MESSAGES;
                cx.notify();
            }
            None => tracing::warn!("ignored a link this app cannot open"),
        }
    }

    /// Something from outside the window, which is already in front.
    pub fn handle(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::Focus => {}
            Request::Url(url) => self.open_link(&url, true, window, cx),
            Request::Open(target) => self.navigate(target, window, cx),
        }
    }

    /// A mention, avatar or member opens the person's profile panel
    /// (`profileTarget` through `openLink`): as a tab beside the open
    /// conversation, or in the companion dock elsewhere.
    pub fn show_profile(
        &mut self,
        pubkey: String,
        _: Point<Pixels>,
        origin: Option<LinkOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = super::profile_panel::target(&pubkey, false) else {
            return;
        };
        if !self.route_link_from(&target, origin, window, cx) {
            self.open_profile(pubkey, window, cx);
        }
    }
}

/// Development only: `BUZZ_DEV_OPEN_LINK=<url>` opens that link's panel
/// beside the first channel the app selects, so headless runs can
/// screenshot it.
fn dev_open_link() -> Option<String> {
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if TAKEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    std::env::var("BUZZ_DEV_OPEN_LINK").ok()
}
