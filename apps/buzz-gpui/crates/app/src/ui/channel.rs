//! The channel page: header, timeline and composer (ui.md §4.1–4.8,
//! §4.12).

use super::channel_menu::{ChannelActions, JoinNotice, Joining};
use super::composer::{Composer, ComposerEvent, Submission};
use super::jump::{self, Jump};
use super::markdown::Directory;
use super::message::{self, Item, Marks, Rows};
use super::message_actions::{Hooks, MessageActions};
use super::reading::{self, Drawn, Reporter};
use super::typing::{self, Activity, AgentChooser};
use super::{scroll, time};
use crate::community::{ChannelModel, Community};
use crate::mirror::mirror;
use buzz_relay_client::fold::MessageRow;
use buzz_relay_client::{ChannelSummary, EventId, TimelineStatus, UnreadDetail, UnreadWatch};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, IconButton, IconButtonSize, IconName,
    PanelHeader, Tab, TabVariant, Tabs, TypeStyled,
};
use chrono::NaiveDate;
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// Older history loads when the reader is this many rows from the top.
const LOAD_OLDER_WITHIN: usize = 5;
/// Arrow keys scroll the history this far (a browser's line step).
const LINE: Pixels = px(40.);
/// The end of the timeline (`.timelineEnd`, `--space-3`).
const END_SPACE: Pixels = px(12.);
/// Older pages read to find a saved reading position's row.
const RESTORE_PAGES: usize = 5;

/// Whether [`ChannelPage::reveal`] found the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// The first page has not arrived yet.
    Loading,
    Shown,
    /// Not a top-level message in the loaded window.
    Absent,
}

/// Asks the workspace to open a thread panel. `reply` is the Reply
/// action, which puts focus in the thread's composer; a summary or link
/// leaves it on the panel.
pub type OpenThread = Arc<dyn Fn(Arc<MessageRow>, bool, &mut Window, &mut App)>;

/// The viewer left or hid this channel; the workspace closes the page.
pub struct ChannelClosed;

pub struct ChannelPage {
    community: Entity<Community>,
    channel: ChannelSummary,
    model: Entity<ChannelModel>,
    items: Vec<Item>,
    /// Row 0 is the top edge (load older); messages follow.
    list: ListState,
    /// The row holding keyboard focus last frame.
    keyboard_row: Option<EventId>,
    /// The edge has shown history controls, so it keeps their space.
    history_space: bool,
    composer: Entity<Composer>,
    input: Entity<TextareaState>,
    /// Row actions: the hover bar, menus, reactions and dialogs.
    message_actions: Entity<MessageActions>,
    error: Option<SharedString>,
    on_open_thread: OpenThread,
    /// The jump pill, and rows that arrived while the reader was away.
    jump: Jump,
    /// The history region, focusable for keyboard scrolling ("Channel
    /// message history"). Focus here or in the composer is reading.
    focus: FocusHandle,
    reporter: Reporter,
    /// Where the history's rows were drawn, for the reporter.
    drawn: Drawn,
    /// Unread state of the rows on screen: thread dots, "Mark read".
    unread_watch: UnreadWatch,
    unread: Arc<UnreadDetail>,
    /// The thread roots and rows `unread_watch` follows.
    watched: (Vec<EventId>, Vec<EventId>),
    /// The viewer's working agents, above the composer.
    activity: Entity<AgentChooser>,
    /// A saved reading position to restore once its rows load.
    restore: Option<scroll::Position>,
    /// Older pages read so far to find the restored row.
    restore_pages: usize,
    /// A revealed message moves focus to the history on the next frame.
    reveal_focus: bool,
    /// The reader started joining this preview.
    joining: bool,
    /// Older history was wanted while the window was out of date; it is
    /// read once the window is verified.
    older_demand: bool,
    /// The local day the rows' day labels were made for.
    day: NaiveDate,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
    /// The header's members button and actions menu.
    actions: Entity<ChannelActions>,
    /// Join for a read-only preview (§4.12).
    join: Entity<JoinNotice>,
    /// The header's tab pane toggle, set by the workspace each frame.
    pub pane: Option<PaneButton>,
    /// Shown inside another channel's tab pane, so without its header.
    pub secondary: bool,
    /// How links in its rows open: a reader's open nowhere in Buzz.
    pub link_mode: crate::ui::links::LinkMode,
    /// The Messages page's conversation, which shows live-update
    /// recovery above its composer (`LiveStatus`).
    pub live_status: bool,
    /// Channel panel launchers for the header, set by the workspace.
    pub launchers: Vec<Launcher>,
    /// Channel panels the header menu opens as tabs, set by the workspace.
    pub menu_panels: Vec<super::channel_menu::MenuPanel>,
    /// The main pane's threads after this conversation, set by the
    /// workspace each frame.
    pub main_tabs: Option<MainTabs>,
    /// The row painted as where a closed thread returned to.
    returned: Option<(EventId, u64)>,
    /// Rows showing agent work (a bubble or a working thread summary),
    /// remeasured when that changes so the tail stays in view.
    working_rows: HashSet<EventId>,
    /// A conversation on the Me page: "Message your agents", and only the
    /// viewer's own agents can be mentioned in.
    personal: bool,
    /// Me's selected assistant, for sends that mention nobody.
    selected_agent: Option<String>,
}

/// A header button toggling a channel panel (`channelLauncher`).
#[derive(Clone)]
pub struct Launcher {
    pub key: &'static str,
    pub icon: IconName,
    /// The tooltip, with the panel's shortcut when it has one.
    pub label: SharedString,
    pub pressed: bool,
    pub toggle: PaneToggle,
}

/// Opens or closes the channel's tab pane.
pub type PaneToggle = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A thread tab in the main pane, after the conversation's own tab.
pub struct MainTab {
    pub id: SharedString,
    pub label: SharedString,
    pub icon: IconName,
    pub close: PaneToggle,
    /// The pointer can drag it to another place or pane.
    pub drag: Option<super::tab_drag::TabDrag>,
}

/// Shows a main-pane tab: a thread, or with `None` the conversation.
pub type SelectMain = Rc<dyn Fn(Option<SharedString>, &mut Window, &mut App)>;

/// The main pane's tab strip (`PanelWorkspace main`): the conversation's
/// permanent tab, then the threads placed there. A lone conversation tab
/// reads as the header's title.
pub struct MainTabs {
    pub tabs: Vec<MainTab>,
    /// The thread shown in place of the conversation.
    pub selected: Option<SharedString>,
    pub content: Option<AnyView>,
    pub select: SelectMain,
    /// Where the strip and its tabs are, for tab drags.
    pub geometry: super::tab_drag::Geometry,
    pub strip: ScrollHandle,
    /// A thread just selected pushes in over what it replaces.
    pub motion: Option<MainMotion>,
}

/// A thread selected in the main pane slides in from the right over what
/// it replaces, which drifts left and dims underneath until it lands.
#[derive(Clone)]
pub struct MainMotion {
    pub started: std::time::Instant,
    /// What it replaces: another thread, or `None` for the conversation.
    pub leave: Option<AnyView>,
}

/// How long the push takes.
const PUSH: std::time::Duration = std::time::Duration::from_millis(300);

impl MainMotion {
    /// Its progress, eased; `None` once it has landed.
    fn progress(&self) -> Option<f32> {
        let t = self.started.elapsed().as_secs_f32() / PUSH.as_secs_f32();
        (t < 1.).then(|| buzz_ui::tokens::easing::cubic_bezier((0.2, 0., 0., 1.), t))
    }
}

/// The push: `leave` drifts left and dims under `enter`, which slides in
/// from the right with the dock's shadow.
fn push(leave: AnyElement, enter: AnyElement, t: f32, cx: &App) -> AnyElement {
    let c = cx.colors();
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .size_full()
                .left(relative(-0.2 * t))
                .opacity(1. - 0.4 * t)
                .child(leave),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .w_full()
                .left(relative(1. - t))
                .flex()
                .flex_col()
                .bg(c.surface_panel)
                .shadow(buzz_ui::tokens::shadow::sm())
                .child(enter),
        )
        .into_any_element()
}

/// The header's "Toggle tab pane" button.
#[derive(Clone)]
pub struct PaneButton {
    /// The pane shows.
    pub open: bool,
    /// The pane has something to show; with nothing the button is
    /// disabled, as there are no blank tabs to open.
    pub enabled: bool,
    pub toggle: PaneToggle,
}

impl EventEmitter<ChannelClosed> for ChannelPage {}

impl ChannelPage {
    pub fn new(
        community: Entity<Community>,
        channel: ChannelSummary,
        on_open_thread: OpenThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // A channel closed a moment ago keeps its measured rows and place;
        // otherwise the saved position is restored once its rows load.
        let (list, items, restore, seed) = match scroll::take(&community, &channel.id, cx) {
            Some(cached) => (cached.list, cached.items, None, Some(cached.snapshot)),
            None => {
                let list = ListState::new(1, ListAlignment::Bottom, px(1000.));
                list.set_follow_mode(FollowMode::Tail);
                let restore = scroll::load(&community, &channel.id, cx);
                (list, Vec::new(), restore, None)
            }
        };
        let model = community.update(cx, |community, cx| {
            community.open_channel(&channel.id, seed, cx)
        });
        let unread_watch = community.read(cx).session.watch_unread(&channel.id);
        let placeholder = composer_placeholder(&channel, &community.read(cx).snapshot.profiles);
        let composer = cx.new(|cx| {
            Composer::new(
                community.clone(),
                channel.id.clone(),
                None,
                placeholder,
                window,
                cx,
            )
        });
        composer.update(cx, |composer, _| composer.conversation_view());
        let input = composer.read(cx).input();
        let focus = cx.focus_handle();
        let message_actions =
            Self::message_actions(&community, &channel, &composer, &on_open_thread, window, cx);
        let mut subscriptions = vec![
            cx.observe(&message_actions, |_, _, cx| cx.notify()),
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.sync_items(window, cx)
            }),
            // Reading stops when the window is in the background.
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
            cx.on_focus_in(&focus, window, |_, _, cx| cx.notify()),
            cx.on_focus_out(&focus, window, |_, _, _, cx| cx.notify()),
            cx.observe_in(&community, window, |this, community, window, cx| {
                // Joining, leaving or editing changes the summary.
                if let Some(current) = community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .find(|c| c.id == this.channel.id)
                {
                    this.channel = current.clone();
                }
                // A join the reader started ends in their composer once the
                // channel is writable.
                if this.joining && !this.channel.read_only {
                    this.joining = false;
                    this.input.update(cx, |state, cx| state.focus(window, cx));
                }
                // A DM is named after its people, whose profiles may arrive
                // later.
                let placeholder = match this.personal {
                    true => "Message your agents".into(),
                    false => {
                        composer_placeholder(&this.channel, &community.read(cx).snapshot.profiles)
                    }
                };
                this.composer.update(cx, |composer, cx| {
                    composer.set_placeholder(placeholder, window, cx)
                });
                cx.notify()
            }),
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &ComposerEvent, window, cx| match event {
                    ComposerEvent::Submit(submission) => this.send(submission.clone(), window, cx),
                    ComposerEvent::EditLast => this.edit_last(window, cx),
                    ComposerEvent::CancelReply => {}
                    ComposerEvent::RetryEdit => this
                        .message_actions
                        .update(cx, |actions, cx| actions.retry_edit(cx)),
                },
            ),
        ];
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            // Row 0 is the edge.
            if event.visible_range.start < LOAD_OLDER_WITHIN + 1 {
                let _ = weak.update(cx, |this, cx| this.load_near_top(cx));
            }
        });
        let page_handle = cx.entity().downgrade();
        let on_removed: super::channel_menu::OnRemoved = std::rc::Rc::new(move |_, cx| {
            let _ = page_handle.update(cx, |_, cx| cx.emit(ChannelClosed));
        });
        let actions =
            cx.new(|_| ChannelActions::new(community.clone(), channel.clone(), on_removed));
        let join = cx.new(|_| JoinNotice::new(community.clone(), channel.clone()));
        subscriptions.push(cx.subscribe(&join, |this, _, _: &Joining, _| this.joining = true));
        let tasks = vec![
            time::on_new_day(cx, |_, cx| cx.notify()),
            mirror(
                unread_watch.snapshot.clone(),
                cx,
                |this: &mut Self, value| this.unread = value,
            ),
        ];
        // The reading position is kept when the page closes, and when the
        // app quits with it open.
        cx.on_release(|this, cx| this.keep_position(cx)).detach();
        cx.on_app_quit(|this, cx| {
            this.save_position(cx);
            async {}
        })
        .detach();
        let unread = unread_watch.snapshot.borrow().clone();
        let mut page = Self {
            actions,
            join,
            community,
            channel,
            model,
            items,
            list,
            keyboard_row: None,
            history_space: false,
            composer,
            input,
            message_actions,
            error: None,
            on_open_thread,
            pane: None,
            secondary: false,
            link_mode: crate::ui::links::LinkMode::Normal,
            live_status: false,
            launchers: Vec::new(),
            menu_panels: Vec::new(),
            main_tabs: None,
            returned: None,
            working_rows: HashSet::new(),
            personal: false,
            selected_agent: None,
            jump: Jump::default(),
            focus,
            reporter: Reporter::default(),
            drawn: Drawn::default(),
            unread,
            unread_watch,
            watched: (Vec::new(), Vec::new()),
            activity: cx.new(AgentChooser::new),
            restore,
            restore_pages: 0,
            reveal_focus: false,
            joining: false,
            older_demand: false,
            day: time::today(),
            _subscriptions: subscriptions,
            _tasks: tasks,
        };
        page.sync_items(window, cx);
        page
    }

    /// Shows the conversation as Me does (`personalConversation`).
    pub fn set_personal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.personal = true;
        self.composer.update(cx, |composer, cx| {
            composer.set_placeholder("Message your agents".into(), window, cx);
            composer.set_chat(None, cx);
            // Me offers your agents whatever the conversation is.
            composer.invite_agents();
        });
    }

    /// Me's controls under the composer (assistant and project).
    pub fn set_chat_controls(&mut self, controls: AnyView, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.set_chat(Some(controls), cx));
    }

    /// Me's selected assistant: a plain send goes to it (`selectedAgent`).
    pub fn set_selected_agent(&mut self, agent: Option<String>) {
        self.selected_agent = agent.filter(|agent| !agent.is_empty());
    }

    /// The header's members button and actions menu.
    pub fn header_actions(&self) -> Entity<ChannelActions> {
        self.actions.clone()
    }

    /// Whether this reader is shown (`ConversationPresentation`).
    pub fn set_presented(this: &Entity<Self>, presented: bool, cx: &mut App) {
        let actions = this.read(cx).message_actions.clone();
        actions.update(cx, |actions, cx| actions.set_presented(presented, cx));
    }

    /// A thread of this channel closed while shown: its root, when its
    /// row is on screen, is painted once so the reader sees where they
    /// returned (`useReturnHighlight`). It is a cue only, never a scroll.
    pub fn highlight_return(&mut self, root: EventId, cx: &mut Context<Self>) {
        if !self.watched.1.contains(&root) {
            return;
        }
        let painting = self.returned.map_or(1, |(_, painting)| painting + 1);
        self.returned = Some((root, painting));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(message::RETURN_HIGHLIGHT)
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.returned.is_some_and(|(_, shown)| shown == painting) {
                    this.returned = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The Markdown of a loaded message of this timeline.
    pub fn row_content(&self, id: &EventId) -> Option<String> {
        self.items
            .iter()
            .find(|item| item.row.id == *id)
            .map(|item| item.row.content.clone())
    }

    pub fn channel_id(&self) -> &str {
        &self.channel.id
    }

    pub fn channel(&self) -> &ChannelSummary {
        &self.channel
    }

    pub fn composer(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    /// Mounts the composer's tools where buzz-app mounts the composer: in
    /// any conversation but a preview, shown or not.
    pub fn mount_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preview() {
            self.composer
                .update(cx, |composer, cx| composer.mount_tools(window, cx));
        }
    }

    /// A conversation that opens no threads, as a session shown flat in
    /// Messages does (`onOpenThread` unset for `flatSession`).
    pub fn without_threads(&mut self, cx: &mut Context<Self>) {
        self.message_actions
            .update(cx, |actions, cx| actions.without_threads(cx));
    }

    /// A reader's conversation: links open nowhere in Buzz and no thread
    /// opens from it (`ChannelPreview` has no `onOpenThread`).
    pub fn as_reader(&mut self, cx: &mut Context<Self>) {
        self.link_mode = crate::ui::links::LinkMode::Closed;
        self.message_actions
            .update(cx, |actions, cx| actions.without_threads(cx));
        // Nor its working agents' threads or activity.
        self.activity.update(cx, |chooser, _| chooser.closed = true);
    }

    /// The composer itself, for hosts that follow its sends (Inbox drafts).
    pub fn composer_entity(&self) -> Entity<super::composer::Composer> {
        self.composer.clone()
    }

    /// The first page has not arrived yet (the launch surface waits).
    pub fn first_page_loading(&self, cx: &App) -> bool {
        self.model.read(cx).snapshot.status == TimelineStatus::Loading
    }

    /// Scrolls a top-level message of the loaded window into view. Any
    /// other message, a reply or one outside the window, belongs in its
    /// thread; there is no "around message" query (relay.md §5c.5). The
    /// choice waits for the window's first page, and a window not yet
    /// confirmed by the relay never holds it (`exactOpening`).
    pub fn reveal(&mut self, id: EventId, cx: &mut Context<Self>) -> Reveal {
        let snapshot = &self.model.read(cx).snapshot;
        if snapshot.status == TimelineStatus::Loading {
            return Reveal::Loading;
        }
        let index = self
            .items
            .iter()
            .position(|item| item.row.id == id && item.row.thread_root_id.is_none())
            .filter(|_| snapshot.status == TimelineStatus::Ready && !snapshot.stale);
        let Some(index) = index else {
            return Reveal::Absent;
        };
        // List row 0 is the edge. The reader is taken to the row and focus
        // moves to the history, with no tint (`useMessageReveal`).
        self.list.scroll_to(ListOffset {
            item_ix: index + 1,
            offset_in_item: px(0.),
        });
        self.restore = None;
        self.reveal_focus = true;
        cx.notify();
        Reveal::Shown
    }

    /// Loads older history when the reader is near the top. While the
    /// window is from before a disconnect the demand is kept, and tried
    /// again once the window is verified.
    fn load_near_top(&mut self, cx: &mut Context<Self>) {
        self.older_demand = false;
        let model = self.model.read(cx);
        let snapshot = &model.snapshot;
        if snapshot.status != TimelineStatus::Ready
            || !snapshot.has_more
            || snapshot.history_limited
            || snapshot.loading_older
            || snapshot.error.is_some()
        {
            return;
        }
        self.older_demand = snapshot.stale;
        model.load_older();
    }

    /// Puts the reader back where they left the channel once its newest
    /// page is in. A row older than that page is looked for in a few older
    /// pages; one that cannot be found leaves the reader at the end.
    fn restore_position(&mut self, cx: &mut Context<Self>) {
        let Some(position) = &self.restore else {
            return;
        };
        let model = self.model.read(cx);
        let snapshot = &model.snapshot;
        if snapshot.status != TimelineStatus::Ready || snapshot.loading_older {
            return;
        }
        if let Some(offset) = scroll::offset(position, &self.items) {
            self.restore = None;
            self.list.scroll_to(offset);
            return;
        }
        let missing = !position.bottom && position.anchor.is_some();
        if missing
            && snapshot.has_more
            && !snapshot.history_limited
            && snapshot.error.is_none()
            && self.restore_pages < RESTORE_PAGES
        {
            self.restore_pages += 1;
            model.load_older();
            return;
        }
        self.restore = None;
    }

    /// Saves where the reader is, for the next time the channel opens.
    fn save_position(&self, cx: &App) {
        if self.items.is_empty() {
            return;
        }
        let position = scroll::capture(&self.list, &self.items);
        scroll::save(&self.community, &self.channel.id, &position, cx);
    }

    /// The page is closing: saves the position and keeps the measured
    /// rows for a quick return.
    fn keep_position(&mut self, cx: &mut App) {
        self.save_position(cx);
        if self.items.is_empty() {
            return;
        }
        let cached = scroll::Cached {
            list: self.list.clone(),
            items: std::mem::take(&mut self.items),
            snapshot: self.model.read(cx).snapshot.clone(),
        };
        scroll::keep(&self.community, &self.channel.id, cached, cx);
    }

    /// After each layout: reports what the reader sees, shows or hides the
    /// jump pill, and follows the unread state of the rows on screen.
    fn after_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = &self.items;
        let view = reading::view(&self.list, &self.drawn, |ix| {
            ix.checked_sub(1)
                .and_then(|ix| items.get(ix))
                .filter(|item| item.row.membership.is_none())
                .map(|item| item.row.id)
        });
        let input = self.input.read(cx).focus_handle(cx);
        let active = window.is_window_active()
            && (self.focus.contains_focused(window, cx) || input.contains_focused(window, cx));
        if let Some((visible, at_bottom)) = self.reporter.next(&view, active) {
            self.model.read(cx).view.viewport(visible, at_bottom);
        }
        let distance = reading::distance_from_bottom(&self.list);
        let away = distance >= jump::DISTANCE;
        if self.jump.shown() != away {
            self.jump.set(away);
            cx.notify();
        }
        let threads: Vec<EventId> = view
            .shown
            .iter()
            .filter(|id| {
                self.items
                    .iter()
                    .any(|item| item.row.id == **id && item.row.reply_count > 0)
            })
            .copied()
            .collect();
        if self.watched.0 != threads || self.watched.1 != view.shown {
            self.unread_watch.set(threads.clone(), view.shown.clone());
            self.watched = (threads, view.shown);
        }
    }

    /// Keyboard scrolling of the focused history (`ChannelTimeline`):
    /// upward keys leave tail follow; End returns to it.
    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keys = &event.keystroke;
        let page = self.list.viewport_bounds().size.height * 0.875;
        let plain = !keys.modifiers.control && !keys.modifiers.alt && !keys.modifiers.platform;
        match keys.key.as_str() {
            "up" => self.list.scroll_by(-LINE),
            "down" => self.list.scroll_by(LINE),
            "pageup" if plain => self.list.scroll_by(-page),
            "pagedown" if plain => self.list.scroll_by(page),
            "space" if plain && keys.modifiers.shift => self.list.scroll_by(-page),
            "space" if plain => self.list.scroll_by(page),
            "home" if plain || keys.modifiers.control => self.list.scroll_to(ListOffset::default()),
            "end" if plain || keys.modifiers.control => self.list.set_follow_mode(FollowMode::Tail),
            _ => return,
        }
        cx.stop_propagation();
        if self.list.logical_scroll_top().item_ix < LOAD_OLDER_WITHIN + 1 {
            self.load_near_top(cx);
        }
        cx.notify();
    }

    /// Rebuilds the rows, telling the list exactly what was added at either
    /// end so the reading position stays put while history loads above.
    fn sync_items(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.model.read(cx).snapshot.rows.clone();
        let next = message::items(&rows);
        let old: Vec<_> = self.items.iter().map(|item| item.row.id).collect();
        let new: Vec<_> = next.iter().map(|item| item.row.id).collect();
        match old
            .first()
            .and_then(|first| new.iter().position(|id| id == first))
        {
            Some(prepended) if new[prepended..].starts_with(&old) => {
                // List indices are one past item indices: row 0 is the edge.
                self.list.splice(1..1, prepended);
                let end = prepended + old.len();
                let appended = new.len() - end;
                self.list.splice(end + 1..end + 1, appended);
                // Head flags of the old first row and the row before the
                // appended tail may have changed, and that row no longer
                // ends the timeline.
                if !old.is_empty() {
                    self.list.splice(prepended + 1..prepended + 2, 1);
                    self.list.splice(end..end + 1, 1);
                }
                if !self.list.is_following_tail() && !old.is_empty() {
                    self.jump.arrived(appended);
                }
            }
            _ => self.list.reset(new.len() + 1),
        }
        // The edge's state may have changed.
        self.list.splice(0..1, 1);
        // Agent avatars show presence, so the session watches their authors.
        let agents = {
            let community = self.community.read(cx);
            let profiles = &community.snapshot.profiles;
            let library = super::identity::AgentLibrary::from_hosts(cx);
            let mut agents: Vec<String> = Vec::new();
            for item in &next {
                let author = item.row.author.to_hex();
                let agent = item.row.is_agent_envelope()
                    || profiles.get(&author).is_some_and(|p| p.is_agent)
                    || library.agents.iter().any(|(key, _, _)| *key == author);
                if agent && item.row.workflow_owner.is_none() && !agents.contains(&author) {
                    agents.push(author);
                }
            }
            agents
        };
        self.community
            .update(cx, |community, _| community.set_presence_rows(agents));
        self.items = next;
        self.day = time::today();
        // A saved position applies once the first rows are in; a reader
        // who was at the bottom, or whose row is gone, starts at the end.
        self.restore_position(cx);
        let snapshot = &self.model.read(cx).snapshot;
        if self.older_demand && !snapshot.stale {
            if self.list.logical_scroll_top().item_ix < LOAD_OLDER_WITHIN + 1 {
                self.load_near_top(cx);
            } else {
                self.older_demand = false;
            }
        }
        cx.notify();
    }

    fn session(&self, cx: &App) -> Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    fn send(&mut self, submission: Submission, window: &mut Window, cx: &mut Context<Self>) {
        crate::perf::begin(crate::perf::SEND);
        if let Some(target) = &submission.editing {
            let current = self
                .items
                .iter()
                .find(|item| item.row.id == target.id)
                .map(|item| item.row.clone());
            self.message_actions.update(cx, |actions, cx| {
                actions.save_edit(submission, current, window, cx)
            });
            return;
        }
        let session = self.session(cx);
        let channel = self.channel.id.clone();
        if submission.editing.is_none() {
            self.list.set_follow_mode(FollowMode::Tail);
            self.restore = None;
            self.jump.set(false);
        }
        let mut mentions = submission.mentions.clone();
        // Sessions pick their recipient and add mentioned agents first.
        let session_send = (self.personal
            || self.channel.channel_type == Some(buzz_relay_client::ChannelType::Session))
        .then(|| {
            let community = self.community.read(cx);
            let known: Vec<String> = super::identity::AgentLibrary::from_hosts(cx)
                .agents
                .into_iter()
                .map(|(key, _, _)| key)
                .collect();
            (
                known,
                community.snapshot.profiles.clone(),
                super::me::recipients::rules(&self.community, cx),
                super::me::pending::draft_key_for(&self.community, &channel, cx).is_some(),
            )
        });
        let personal = self.personal;
        // In Me a message naming nobody goes to the selected assistant.
        let selected = self
            .selected_agent
            .clone()
            .filter(|_| personal && mentions.recipients.is_empty());
        // With no recipients to resolve and no files to upload, the intent
        // is queued now, so the row shows on the next frame.
        let queued = (session_send.is_none() && submission.upload.is_none()).then(|| {
            session.send(
                &channel,
                &submission.content,
                mentions.clone(),
                Vec::new(),
                None,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Some((known, profiles, rules, starting)) = session_send
                && submission.editing.is_none()
            {
                let prepared = if starting {
                    Err("Finish setting up this conversation in Me before sending messages.".into())
                } else {
                    let mut explicit = mentions.recipients.clone();
                    explicit.append(&mut mentions.references);
                    super::me::recipients::prepare(
                        &session, &channel, explicit, selected, personal, known, profiles, rules,
                    )
                    .await
                };
                match prepared {
                    Ok(recipients) => mentions.recipients = recipients,
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            this.error = Some(error.into());
                            this.composer.update(cx, |composer, cx| composer.failed(cx));
                            cx.notify();
                        });
                        return;
                    }
                }
            }
            let result = match (&submission.editing, queued) {
                (Some(row), _) => session.edit(&channel, row.id, &submission.content).await,
                (None, Some(queued)) => queued.await,
                (None, None) => {
                    // Files upload in the background first; a failed upload
                    // is handed back to the composer's draft.
                    let Some(attachments) = submission.attachments().await else {
                        return;
                    };
                    session
                        .send(&channel, &submission.content, mentions, attachments, None)
                        .await
                }
            };
            let handed_back = cx
                .update(|_, cx| submission.settle(&result, cx))
                .unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.error = None;
                        this.composer
                            .update(cx, |composer, cx| composer.sent(window, cx));
                    }
                    Err(_) if handed_back => {}
                    Err(error) => {
                        this.error = Some(error.into());
                        this.composer.update(cx, |composer, cx| composer.failed(cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_edit(&mut self, row: Arc<MessageRow>, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.start_edit(row, window, cx));
    }

    /// Up in an empty composer edits the viewer's newest editable message
    /// (`lastEditableMessage`).
    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self
            .message_actions
            .read(cx)
            .last_editable(self.items.iter().map(|item| &item.row), cx);
        if let Some(row) = row {
            self.start_edit(row, window, cx);
        }
    }

    /// The row actions, wired to this page's composer and thread panel.
    fn message_actions(
        community: &Entity<Community>,
        channel: &ChannelSummary,
        composer: &Entity<Composer>,
        on_open_thread: &OpenThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<MessageActions> {
        let page = cx.entity().downgrade();
        let (edit_composer, focus_composer) = (composer.downgrade(), composer.downgrade());
        let (reply, open) = (on_open_thread.clone(), on_open_thread.clone());
        let hooks = Hooks {
            reply: Rc::new(move |row, window, cx| reply(row.clone(), true, window, cx)),
            open_thread: Some(Rc::new(move |row, window, cx| {
                open(row.clone(), false, window, cx)
            })),
            edit: Rc::new(move |row, window, cx| {
                let _ =
                    edit_composer.update(cx, |composer, cx| composer.start_edit(row, window, cx));
            }),
            focus_composer: Rc::new(move |window, cx| {
                let _ = focus_composer.update(cx, |composer, cx| composer.focus(window, cx));
            }),
            thread_root: None,
            find: Rc::new(move |id, cx| {
                page.upgrade().and_then(|page| {
                    page.read(cx)
                        .items
                        .iter()
                        .find(|item| item.row.id == id)
                        .map(|item| item.row.clone())
                })
            }),
            thread: None,
            composer: composer.downgrade(),
        };
        let (community, channel) = (community.clone(), channel.id.clone());
        cx.new(|cx| MessageActions::new(community, channel, hooks, window, cx))
    }

    /// The list's first row: the window's error, then load older, its
    /// progress, its retry, or the history window limit (§4.3). It is 16px
    /// tall, or a control's height more once `history_space` reserves it.
    fn edge(
        snapshot: &buzz_relay_client::TimelineSnapshot,
        history_space: bool,
        model: Entity<ChannelModel>,
        cx: &App,
    ) -> AnyElement {
        let c = cx.colors();
        let error = snapshot.error.clone().map(|error| div().child(error));
        let frame = || {
            div()
                .min_h(px(if history_space { 56. } else { 16. }))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_caption()
                .text_color(c.text_subtle)
        };
        if snapshot.history_limited {
            return frame()
                .children(error)
                .child("History window limit reached")
                .into_any_element();
        }
        if !snapshot.has_more {
            return frame().children(error).into_any_element();
        }
        let label = if snapshot.loading_older {
            "Loading older…"
        } else if snapshot.error.is_some() {
            "Retry older messages"
        } else {
            "Load older messages"
        };
        frame()
            .children(error)
            .child(
                Button::new("load-older", label)
                    .variant(ButtonVariant::Subtle)
                    .size(ButtonSize::Md)
                    .disabled(snapshot.loading_older)
                    .on_click(move |_, _, cx| model.read(cx).load_older()),
            )
            .into_any_element()
    }

    /// The timeline's place while it has no rows (`ChannelBody`): loading,
    /// or the first page's error with "Retry messages".
    fn placeholder(
        &self,
        snapshot: &buzz_relay_client::TimelineSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let empty = div()
            .flex_1()
            .flex()
            .flex_col()
            .items_start()
            .gap(px(12.))
            .py(px(32.))
            .px(px(16.))
            .text_body_sm()
            .text_color(c.text_subtle);
        match &snapshot.status {
            TimelineStatus::Error(error) => empty
                .child(error.clone())
                .child(
                    Button::new("retry-messages", "Retry messages")
                        .size(ButtonSize::Md)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.read(cx).view.retry();
                        })),
                )
                .into_any_element(),
            _ => empty.child("Loading messages…").into_any_element(),
        }
    }

    /// A public preview: read-only and confirmed by the relay. It has a
    /// Join prompt over its history instead of a composer.
    fn preview(&self) -> bool {
        self.channel.read_only && !self.channel.cached
    }

    /// The composer, with live-update recovery above it (`LiveStatus`),
    /// and who is typing over its top edge. A preview has no composer.
    fn composer_shell(&self, typing: Option<AnyElement>, cx: &mut Context<Self>) -> AnyElement {
        let read_only = self.channel.read_only;
        self.composer
            .update(cx, |composer, cx| composer.set_can_send(!read_only, cx));
        let preview = self.preview();
        let composer = self.composer.downgrade();
        let history = self.focus.clone();
        let live = self
            .live_status
            .then(|| {
                super::live_status::recovery(
                    &self.community,
                    Some(&self.channel.id),
                    // Retry removes the focused button: focus moves on, to
                    // the history where there is no composer.
                    move |window, cx| match preview {
                        true => window.focus(&history, cx),
                        false => {
                            let _ = composer.update(cx, |composer, cx| composer.focus(window, cx));
                        }
                    },
                    cx,
                )
            })
            .flatten();
        // A channel known only from the cache shows its disabled composer
        // with the draft.
        div()
            .flex()
            .flex_col()
            .children(live)
            .when(!preview, |this| {
                this.child(
                    div()
                        .relative()
                        .children(typing)
                        .child(self.composer.clone()),
                )
            })
            .into_any_element()
    }
}

/// `MessageComposer`'s label: `Message #<name>`, where a DM's name is its
/// people (or "Notes to self"), still with the `#` as in buzz-app.
fn composer_placeholder(
    channel: &ChannelSummary,
    profiles: &buzz_relay_client::Profiles,
) -> SharedString {
    match channel.channel_type {
        Some(buzz_relay_client::ChannelType::Session) => "Message this session".into(),
        _ => format!(
            "Message #{}",
            super::sidebar::channel_label(channel, profiles)
        )
        .into(),
    }
}

impl Render for ChannelPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let menu_panels = self.menu_panels.clone();
        self.actions
            .update(cx, |actions, _| actions.menu_panels = menu_panels);
        // "Today" became "Yesterday": the day labels are rebuilt.
        if self.day != time::today() {
            self.day = time::today();
            self.items = message::items(&self.model.read(cx).snapshot.rows);
        }
        if std::mem::take(&mut self.reveal_focus) {
            window.focus(&self.focus, cx);
        }
        let snapshot = self.model.read(cx).snapshot.clone();
        // Once the edge shows a history control it keeps that space, so
        // removing the control cannot move the reader's anchor as older
        // rows arrive.
        self.history_space |= snapshot.has_more
            || snapshot.loading_older
            || snapshot.history_limited
            || snapshot.error.is_some();
        let history_space = self.history_space;
        if crate::perf::on() && !self.items.is_empty() {
            use crate::perf::{STARTUP, SWITCH, shown};
            shown(SWITCH, "timeline");
            match snapshot.restored {
                true => shown(STARTUP, "timeline.cached"),
                false => shown(STARTUP, "timeline.live"),
            }
        }
        let narrow = window.viewport_size().width <= typing::NARROW;
        let inset = if narrow { px(12.) } else { px(24.) };
        let owned = typing::owned_agents(cx);
        let community = self.community.read(cx);
        let directory = Directory::of(community, Some(&self.channel.members), cx);
        let names = directory.names.clone();
        let viewer = community.session.viewer().to_string();
        if crate::perf::on() {
            probe_send(&self.items, &viewer);
        }

        // Who is typing, and which of the viewer's agents work here.
        let roots: HashSet<String> = self.items.iter().map(|item| item.row.id.to_hex()).collect();
        let mut activity = Activity::channel(
            community
                .snapshot
                .typing
                .get(&self.channel.id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            &community.snapshot.thread_typing,
            |root| roots.contains(root),
            &owned,
        )
        .observe(crate::agent_activity::observer::observed(
            &self.community,
            &self.channel.id,
            None,
            cx,
        ));
        let observed = crate::agent_activity::observer::snapshot(&self.community, cx);
        // Message bubbles stand in for the composer's line
        // (`ActivityTypingReplacement`).
        activity.replace(&crate::agent_activity::request::bubbled_agents(
            &observed,
            &self.channel.id,
            None,
        ));
        let (working, waiting) = thread_work(
            &observed,
            &self.channel.id,
            &community.snapshot.thread_typing,
            &roots,
            &owned,
        );
        let profiles = community.snapshot.profiles.clone();
        let media = community.session.media().clone();
        let pane = self.pane.clone().map(|pane| {
            let toggle = pane.toggle;
            IconButton::new("toggle-tab-pane", IconName::SidebarRight)
                .size(IconButtonSize::Sm)
                .tooltip(if pane.open {
                    "Close tab pane"
                } else {
                    "Open tab pane"
                })
                .selected(pane.open)
                .disabled(!pane.enabled)
                .on_click(move |_, window, cx| toggle(window, cx))
        });
        let launchers = self.launchers.iter().map(|launcher| {
            let toggle = launcher.toggle.clone();
            IconButton::new(
                SharedString::from(format!("launch-{}", launcher.key)),
                launcher.icon,
            )
            .size(IconButtonSize::Sm)
            .tooltip(launcher.label.clone())
            .selected(launcher.pressed)
            .on_click(move |_, window, cx| toggle(window, cx))
        });
        let main = self.main_tabs.as_ref().filter(|main| !main.tabs.is_empty());
        let shown_thread = main.and_then(|main| main.content.clone());
        let header = (!self.secondary).then(|| {
            let icon = message::channel_icon(&self.channel);
            let name = super::sidebar::channel_label(&self.channel, &profiles);
            let header = PanelHeader::new().joined(true);
            let header = match main {
                Some(main) => header.leading(main_strip(main, icon, name)),
                None => header.label(Some(icon), name),
            };
            let header = header
                .action(self.actions.clone())
                .action(div().flex().gap(rems(0.375)).children(launchers))
                .when_some(pane, |header, pane| header.action(pane));
            // The header is where tab drags land in the main pane.
            div()
                .relative()
                .child(header)
                .when_some(self.main_tabs.as_ref(), |this, main| {
                    this.child(super::tab_drag::record(
                        &main.geometry,
                        |geometry, bounds| {
                            geometry
                                .headers
                                .insert(super::tab_drag::PaneKey::Main, bounds);
                        },
                    ))
                })
        });
        // A thread selected in the main pane covers the conversation, whose
        // reading position and draft stay as they were. A newly selected
        // one pushes in over what it replaces.
        let motion = main
            .and_then(|main| main.motion.clone())
            .and_then(|motion| motion.progress().map(|t| (motion.leave, t)));
        if motion.is_some() {
            window.request_animation_frame();
        }
        let pushing_conversation = shown_thread.is_some() && matches!(motion, Some((None, _)));
        if let Some(thread) = shown_thread.clone().filter(|_| !pushing_conversation) {
            let content = match motion {
                Some((Some(leave), t)) => {
                    push(leave.into_any_element(), thread.into_any_element(), t, cx)
                }
                _ => div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(thread)
                    .into_any_element(),
            };
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .h_full()
                .text_body_sm()
                .text_color(c.text_standard)
                .children(header)
                .child(content)
                .into_any_element();
        }

        {
            let items = self.items.clone();
            let open = self.on_open_thread.clone();
            let lookup = items.clone();
            self.activity.update(cx, |chooser, cx| {
                chooser.set(
                    &activity,
                    (&self.channel.id, None),
                    (&profiles, &names, &media),
                    |root| {
                        items
                            .iter()
                            .find(|item| item.row.id.to_hex() == root)
                            .map(|item| item.row.content.clone())
                    },
                    Rc::new(move |thread, window, cx| {
                        let row = thread.and_then(|thread| {
                            lookup
                                .iter()
                                .find(|item| item.row.id == thread)
                                .map(|item| item.row.clone())
                        });
                        if let Some(row) = row {
                            open(row, false, window, cx);
                        }
                    }),
                    cx,
                )
            });
        }

        let unread = self.unread.clone();
        let chat = self.personal;
        let mut rows = Rows::new(viewer, directory);
        // Me's chats put the viewer's own messages on the right.
        rows.align_viewer = chat;
        let actions = self.message_actions.clone();
        MessageActions::fill(&actions, &mut rows, cx);
        rows.interaction.focused = super::message::keyboard_row(rows.in_thread, window);
        // A row the keyboard reaches scrolls into view, as focus does.
        if rows.interaction.focused != self.keyboard_row {
            self.keyboard_row = rows.interaction.focused;
            if let Some(index) = self
                .keyboard_row
                .and_then(|id| self.items.iter().position(|item| item.row.id == id))
            {
                // List row 0 is the edge. Moving stops following the tail,
                // which would pin the list back to the end.
                let before = self.list.logical_scroll_top();
                self.list.scroll_to_reveal_item(index + 1);
                let after = self.list.logical_scroll_top();
                if (after.item_ix, after.offset_in_item) != (before.item_ix, before.offset_in_item)
                {
                    self.list.scroll_to(after);
                }
            }
        }
        let working_rows: HashSet<EventId> = rows
            .bubbles
            .iter()
            .flat_map(|bubbles| bubbles.works.keys().copied())
            .chain(working.keys().copied())
            .collect();
        if working_rows != self.working_rows {
            for id in working_rows.symmetric_difference(&self.working_rows) {
                if let Some(index) = self.items.iter().position(|item| item.row.id == *id) {
                    self.list.splice(index + 1..index + 2, 1);
                }
            }
            self.working_rows = working_rows;
        }
        rows.marks = Marks {
            threads: unread.threads.clone(),
            working,
            waiting,
            returned: self.returned,
        };
        let rows = Rc::new(rows);
        let items = self.items.clone();
        let drawn = self.drawn.clone();
        let weak = cx.entity().downgrade();
        let after_layout = canvas(
            move |_, window, cx| {
                let _ = weak.update(cx, |this, cx| this.after_layout(window, cx));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0();
        // Rows are drawn in layout, so they take the link mode with them.
        let link_mode = self.link_mode;
        // A preview's Join prompt waits for its content, then floats over
        // the history's end, which keeps room for it.
        let preview = self.preview();
        if preview {
            let ready = !self.items.is_empty()
                || matches!(
                    snapshot.status,
                    TimelineStatus::Ready | TimelineStatus::Error(_)
                );
            self.join.update(cx, |join, cx| join.set_ready(ready, cx));
        }
        let preview_inset = match preview {
            true => self.join.read(cx).inset(),
            false => px(0.),
        };
        let timeline = if self.items.is_empty() && snapshot.status != TimelineStatus::Ready {
            self.placeholder(&snapshot, cx)
        } else if self.items.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px(inset)
                .child(Self::edge(&snapshot, history_space, self.model.clone(), cx))
                .when(!snapshot.has_more, |d| {
                    d.child(
                        div()
                            .py(px(32.))
                            .px(px(16.))
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child("No messages yet."),
                    )
                })
                .into_any_element()
        } else {
            let model = self.model.clone();
            let edge_snapshot = snapshot.clone();
            let count = items.len();
            let lane = typing::has_agents(&self.activity, cx);
            let jump = self.jump.render(
                "jump-to-latest",
                lane,
                preview_inset,
                cx.listener(|this, _, window, cx| {
                    this.list.set_follow_mode(FollowMode::Tail);
                    this.restore = None;
                    this.jump.set(false);
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
                cx,
            );
            div()
                .id("channel-history")
                .aria_label("Channel message history")
                .track_focus(&self.focus)
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if !this.focus.contains_focused(window, cx) {
                            window.focus(&this.focus, cx);
                        }
                    }),
                )
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.key(event, cx)))
                .child(
                    list(self.list.clone(), move |index, _, cx| {
                        if index == 0 {
                            return measure(
                                chat,
                                div().px(inset).child(Self::edge(
                                    &edge_snapshot,
                                    history_space,
                                    model.clone(),
                                    cx,
                                )),
                            );
                        }
                        match items.get(index - 1) {
                            Some(item) => measure(
                                chat,
                                div()
                                    .relative()
                                    .px(inset)
                                    .child(crate::ui::links::with_mode(link_mode, || {
                                        message::render(item, &rows, cx)
                                    }))
                                    .child(drawn.probe(index))
                                    // The end space scrolls with the history,
                                    // clear of a preview's Join prompt.
                                    .when(index == count, |d| d.pb(END_SPACE + preview_inset)),
                            ),
                            None => div().into_any_element(),
                        }
                    })
                    .size_full(),
                )
                .child(after_layout)
                .children(jump)
                .into_any_element()
        };

        let timeline = match preview {
            true => div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(timeline)
                .child(self.join.clone())
                .into_any_element(),
            false => timeline,
        };

        // A saved conversation shows no one typing (`MessageComposer`).
        let typing = (!self.channel.cached)
            .then(|| {
                typing::overlay(
                    &activity.humans,
                    Some(self.activity.clone()),
                    &names,
                    narrow,
                    cx,
                )
            })
            .flatten();
        let composer = self.composer_shell(typing, cx);

        let overlays = self
            .message_actions
            .update(cx, |actions, cx| actions.overlays(window, cx));
        let status = self
            .message_actions
            .update(cx, |actions, cx| actions.status(cx));

        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(timeline)
            .children(self.error.clone().map(|error| {
                div()
                    .mx(px(16.))
                    .mb(px(8.))
                    .text_caption()
                    .text_color(c.text_warning)
                    .child(error)
            }))
            .children(status)
            .child(composer)
            .children(overlays);
        // A thread pushing in over the conversation, which drifts under it.
        let body = match (shown_thread, motion) {
            (Some(thread), Some((_, t))) => {
                push(body.into_any_element(), thread.into_any_element(), t, cx)
            }
            _ => body.into_any_element(),
        };
        // The whole pane takes dropped files for the composer.
        super::composer::drop_zone(div(), &self.composer, cx)
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .text_body_sm()
            .text_color(c.text_standard)
            // The agent list closes on Escape before the composer sees it.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.activity.read(cx).is_open() {
                    this.activity.update(cx, |chooser, cx| chooser.close(cx));
                    cx.stop_propagation();
                }
            }))
            .children(header)
            .child(body)
            .children(super::composer::pane_drop_overlay(&self.composer, cx))
            .into_any_element()
    }
}

/// Me's chat transcript is centered at a reading measure
/// (`data-chat-presentation`, 52rem); other conversations use the width.
fn measure(chat: bool, row: Div) -> AnyElement {
    if chat {
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(row.w_full().max_w(rems(52.)))
            .into_any_element()
    } else {
        row.into_any_element()
    }
}

/// The main pane's tab strip: the conversation, selected while no thread
/// is, then the threads placed there, each closable.
fn main_strip(main: &MainTabs, icon: IconName, name: impl Into<SharedString>) -> impl IntoElement {
    let select = main.select.clone();
    let mut strip = Tabs::new(TabVariant::Navigation).tab(
        Tab::new("main-tab-conversation", name)
            .icon(icon)
            .selected(main.selected.is_none())
            .on_click(move |_, window, cx| select(None, window, cx))
            .extend(super::tab_drag::tab_handlers(
                &main.geometry,
                super::tab_drag::CONVERSATION.into(),
                None,
            )),
    );
    for tab in &main.tabs {
        let select = main.select.clone();
        let id = tab.id.clone();
        let close = tab.close.clone();
        strip = strip.tab(
            Tab::new(
                SharedString::from(format!("main-tab-{}", tab.id)),
                tab.label.clone(),
            )
            .icon(tab.icon)
            .selected(main.selected.as_ref() == Some(&tab.id))
            .on_click(move |_, window, cx| select(Some(id.clone()), window, cx))
            .on_close(move |_, window, cx| close(window, cx))
            .draggable(tab.drag.is_some())
            .extend(super::tab_drag::tab_handlers(
                &main.geometry,
                tab.id.to_string(),
                tab.drag.clone(),
            )),
        );
    }
    div()
        .id("main-tab-strip")
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .track_scroll(&main.strip)
        .child(strip)
}

/// Times a send from Enter to its optimistic row and the relay's answer.
fn probe_send(items: &[message::Item], viewer: &str) {
    use crate::perf::{SEND, logged, shown};
    let Some(own) = items
        .iter()
        .rev()
        .find(|item| item.row.author.to_hex() == viewer)
    else {
        return;
    };
    let sending = own
        .row
        .delivery
        .as_ref()
        .is_some_and(|d| d.status == buzz_relay_client::fold::Delivery::Sending);
    if sending {
        shown(SEND, "optimistic");
    } else if logged(SEND, "optimistic") {
        shown(SEND, "confirmed");
    }
}

/// The viewer's agents working in each shown thread, by root, and the
/// threads where all of them are still starting (`useThreadAgents`): the
/// same request state as the bubbles, with a root's own bubble standing
/// for the agents it shows. Typing counts for agents with no known
/// request there.
fn thread_work(
    observed: &crate::agent_activity::Snapshot,
    channel: &str,
    thread_typing: &HashMap<String, Vec<String>>,
    shown: &HashSet<String>,
    owned: &HashSet<String>,
) -> (HashMap<EventId, Vec<String>>, HashSet<EventId>) {
    use crate::agent_activity::request::{
        message_activity, thread_candidates, thread_waiting, thread_working_agents,
    };
    let mut threads = thread_candidates(observed, channel);
    threads.extend(thread_typing.keys().cloned());
    let (mut working, mut waiting) = (HashMap::new(), HashSet::new());
    for root in threads.iter().filter(|root| shown.contains(*root)) {
        let Ok(id) = EventId::from_hex(root) else {
            continue;
        };
        let typing: Vec<String> = thread_typing
            .get(root)
            .into_iter()
            .flatten()
            .filter(|person| owned.contains(*person))
            .cloned()
            .collect();
        let bubble: HashSet<String> = message_activity(observed, channel, root)
            .into_iter()
            .map(|work| work.agent)
            .collect();
        let represented = Some((root.as_str(), &bubble));
        let agents: Vec<String> =
            thread_working_agents(observed, &typing, channel, root, represented)
                .into_iter()
                .filter(|agent| owned.contains(agent))
                .collect();
        if agents.is_empty() {
            continue;
        }
        if thread_waiting(observed, &agents, channel, root, represented) {
            waiting.insert(id);
        }
        working.insert(id, agents);
    }
    (working, waiting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn dms_are_named_after_their_people() {
        use buzz_relay_client::{ChannelType, Profile};
        use std::collections::HashMap;
        let (bob, carol) = ("b".repeat(64), "c".repeat(64));
        let named = |name: &str| {
            Arc::new(Profile {
                name: name.into(),
                ..Default::default()
            })
        };
        let profiles: buzz_relay_client::Profiles = Arc::new(HashMap::from([
            (bob.clone(), named("Bob")),
            (carol.clone(), named("Carol")),
        ]));
        let channel = |kind, participants: Option<Vec<String>>| ChannelSummary {
            name: "DM".into(),
            channel_type: Some(kind),
            participants,
            ..Default::default()
        };
        let group = channel(ChannelType::Dm, Some(vec![bob, carol]));
        assert_eq!(
            composer_placeholder(&group, &profiles),
            "Message #Bob, Carol"
        );
        let notes = channel(ChannelType::Dm, Some(Vec::new()));
        assert_eq!(
            composer_placeholder(&notes, &profiles),
            "Message #Notes to self"
        );
        let stream = ChannelSummary {
            name: "general".into(),
            ..channel(ChannelType::Stream, None)
        };
        assert_eq!(composer_placeholder(&stream, &profiles), "Message #general");
        let session = channel(ChannelType::Session, None);
        assert_eq!(
            composer_placeholder(&session, &profiles),
            "Message this session"
        );
    }
}
