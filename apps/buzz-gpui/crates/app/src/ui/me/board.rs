//! A conversation open in Me (`SessionsPage.tsx` `SessionWork`): its
//! heading with Share, the session actions and the tab pane toggle, the
//! conversation itself, and the tab pane beside it, as on the Messages
//! board but with Me's own tabs.

use super::MeModel;
use super::controls::{self, AssistantChosen, ChatControls};
use super::dialogs::{self, DialogHost};
use crate::community::Community;
use crate::ui::channel::{ChannelPage, OpenThread};
use crate::ui::channel_panels::{self, ChannelPanel};
use crate::ui::channel_tab_views::{
    self, TabHost, TabView, root_content, tab_icon, thread_channel, thread_label, usable,
};
use crate::ui::channel_tabs::{self, PanelTab, TabKind, TabPane, TabState, ThreadTarget};
use crate::ui::message::channel_icon;
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::panels::{self, Close, Dock, TabItem};
use crate::ui::tab_drag::{self, PaneOrder, Rules, TabDrag, TabGeometry};
use crate::ui::thread::{self, ThreadPanel};
use crate::ui::workspace::layout::Split;
use buzz_relay_client::ChannelSummary;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    PanelHeader, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A channel panel opened from its header launcher.
struct Drawer {
    key: &'static str,
    placement: channel_panels::Placement,
    view: AnyView,
}

pub struct SessionWork {
    community: Entity<Community>,
    model: Entity<MeModel>,
    channel: ChannelSummary,
    page: Entity<ChannelPage>,
    /// The assistant and project controls under the composer.
    controls: Entity<ChatControls>,
    thread: Option<Entity<ThreadPanel>>,
    tab_views: HashMap<String, TabView>,
    tab_strip: ScrollHandle,
    revealed_tab: Option<String>,
    /// Where tabs were laid out, for reordering them by drag.
    tab_geometry: tab_drag::Geometry,
    drag_target: Option<tab_drag::DropTarget>,
    drag_cancelled: bool,
    drawer: Option<Drawer>,
    menu_open: bool,
    dialog: Option<dialogs::Dialog>,
    width: Option<f32>,
    opening: u64,
    _subscriptions: Vec<Subscription>,
}

impl TabHost for SessionWork {
    fn community(&self) -> Option<Entity<Community>> {
        Some(self.community.clone())
    }

    fn channel_id(&self, _: &App) -> Option<String> {
        Some(self.channel.id.clone())
    }

    fn page(&self) -> Option<Entity<ChannelPage>> {
        Some(self.page.clone())
    }

    /// Me's conversations host threads in the side pane only.
    fn thread_pane(&self, _: &App) -> TabPane {
        TabPane::Side
    }

    fn tab_views(&mut self) -> &mut HashMap<String, TabView> {
        &mut self.tab_views
    }

    fn tabs(&self, cx: &App) -> TabState {
        SessionWork::tabs(self, cx)
    }

    fn update_tabs(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut TabState)) {
        SessionWork::update_tabs(self, cx, f)
    }

    fn close_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        SessionWork::close_tab(self, id, window, cx)
    }

    fn focus_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        SessionWork::focus_tab(self, id, window, cx)
    }

    fn panel_view(
        &self,
        panel: &ChannelPanel,
        channel: String,
        target: String,
        root: Option<String>,
        close: channel_panels::Close,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyView> {
        Some(SessionWork::panel_view(
            self, panel, channel, target, root, close, window, cx,
        ))
    }
}

impl SessionWork {
    /// `focus`: the composer takes the focus.
    pub fn new(
        community: Entity<Community>,
        channel: ChannelSummary,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = MeModel::global(cx);
        let weak = cx.entity().downgrade();
        let open_thread: OpenThread = Arc::new(move |row, _, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_thread(
                    ThreadTarget {
                        root: row.id,
                        exact: false,
                    },
                    window,
                    cx,
                )
            });
        });
        let page = cx.new(|cx| {
            let mut page =
                ChannelPage::new(community.clone(), channel.clone(), open_thread, window, cx);
            page.secondary = true;
            page.set_personal(window, cx);
            page
        });
        page.read(cx)
            .header_actions()
            .update(cx, |actions, _| actions.members_only = true);
        let assistant = controls::read_assistant(&community, &channel.id, cx);
        let controls = cx.new(|cx| ChatControls {
            assistant: cx
                .new(|_| controls::AssistantControl::new(community.clone(), assistant.clone())),
            project: cx.new(|cx| {
                controls::ProjectControl::chat(community.clone(), channel.id.clone(), cx)
            }),
        });
        page.update(cx, |page, cx| {
            page.set_selected_agent(Some(assistant));
            page.set_chat_controls(controls.clone().into(), cx);
        });
        let assistant = controls.read(cx).assistant.clone();
        let subscriptions = vec![
            cx.subscribe(&assistant, |this, _, AssistantChosen(agent), cx| {
                controls::write_assistant(&this.community, &this.channel.id, agent, cx);
                let agent = agent.clone();
                this.page
                    .update(cx, |page, _| page.set_selected_agent(Some(agent)));
            }),
            cx.observe(&community, |this, community, cx| {
                if let Some(current) = community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .find(|c| c.id == this.channel.id)
                {
                    this.channel = current.clone();
                }
                cx.notify();
            }),
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            community,
            model,
            channel,
            page,
            controls,
            thread: None,
            tab_views: HashMap::new(),
            tab_strip: ScrollHandle::new(),
            tab_geometry: Default::default(),
            drag_target: None,
            drag_cancelled: false,
            revealed_tab: None,
            drawer: None,
            menu_open: false,
            dialog: None,
            width: None,
            opening: 0,
            _subscriptions: subscriptions,
        };
        // The conversation gets back the thread it had.
        if let Some(thread) = this.tabs(cx).thread {
            this.make_thread(thread, window, cx);
        }
        if focus {
            this.page.update(cx, |page, cx| {
                let composer = page.composer();
                composer.update(cx, |state, cx| state.focus(window, cx));
            });
        }
        this
    }

    /// Shows panel tab `id`, as it is.
    pub(crate) fn show_panel_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.update_tabs(cx, |state| state.select(id));
        self.focus_tab(id, window, cx);
    }

    /// Panel tab `id` is open here.
    pub(crate) fn has_panel_tab(&self, id: &str, cx: &App) -> bool {
        self.tabs(cx).panels.iter().any(|tab| tab.id() == id)
    }

    /// When `view` is panel tab `id` here, whether that tab is not the
    /// one selected.
    pub(crate) fn panel_view_unselected(
        &self,
        id: &str,
        view: gpui_kit::EntityId,
        cx: &App,
    ) -> Option<bool> {
        matches!(self.tab_views.get(id), Some(TabView::Panel(shown)) if shown.entity_id() == view)
            .then(|| self.tabs(cx).selected != id)
    }

    /// A link from the timeline replaces the panels links opened before
    /// (`open(..., fromThread = false)`).
    pub(crate) fn retire_link_panels(&mut self, cx: &mut Context<Self>) {
        self.update_tabs(cx, |state| state.panels.retain(|panel| !panel.transient()));
    }

    /// Opens panel `key` (or the one that shows `target`) as a tab of
    /// this conversation, as `useConversationTabs` `open` does.
    pub(crate) fn open_panel(
        &mut self,
        key: Option<&str>,
        target: &str,
        channel: Option<String>,
        root: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let panel = match key {
            Some(key) => channel_panels::get(key, cx),
            None => channel_panels::resolve(target, cx),
        };
        let Some(panel) = panel else {
            return false;
        };
        let tab = PanelTab {
            panel: panel.key,
            channel: channel.unwrap_or_else(|| self.channel.id.clone()),
            target: target.to_string(),
            root,
            tool: false,
        };
        let id = tab.id();
        if self.update_tabs(cx, |state| state.open_panel(tab)) {
            self.tab_views.remove(&id);
            self.ensure_tab_views(window, cx);
        }
        self.focus_tab(&id, window, cx);
        true
    }

    /// Tab `to` replaced tool `from` here (`panel/open`).
    pub(crate) fn inherit_tool(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        self.update_tabs(cx, |state| state.inherit_tool(from, to));
    }

    /// Panel `key` was taken away: its tabs and drawer here close.
    pub(crate) fn withdraw_panel(&mut self, key: &str, cx: &mut Context<Self>) {
        let prefix = format!("{key}:");
        self.update_tabs(cx, |state| {
            state.panels.retain(|tab| tab.panel != key);
            if state.selected.starts_with(&prefix) {
                state.selected = String::new();
            }
        });
        if self.drawer.as_ref().is_some_and(|drawer| drawer.key == key) {
            self.drawer = None;
        }
        cx.notify();
    }

    /// A panel tab here now shows `new` in place of `old`.
    pub(crate) fn retarget_panel(
        &mut self,
        panel: &str,
        view: gpui_kit::EntityId,
        old: &str,
        new: &str,
        cx: &mut Context<Self>,
    ) {
        let (old_id, new_id) = (format!("{panel}:{old}"), format!("{panel}:{new}"));
        // Only `view`'s own tab follows.
        if !matches!(self.tab_views.get(&old_id), Some(TabView::Panel(shown)) if shown.entity_id() == view)
        {
            return;
        }
        if !self.tabs(cx).panels.iter().any(|tab| tab.id() == new_id)
            && let Some(view) = self.tab_views.remove(&old_id)
        {
            self.tab_views.insert(new_id.clone(), view);
        }
        self.update_tabs(cx, |state| {
            if state.panels.iter().any(|tab| tab.id() == new_id) {
                state.panels.retain(|tab| tab.id() != old_id);
            } else if let Some(tab) = state.panels.iter_mut().find(|tab| tab.id() == old_id) {
                tab.target = new.to_owned();
            }
            if state.selected == old_id {
                state.selected = new_id.clone();
            }
        });
    }

    fn tabs(&self, cx: &App) -> TabState {
        self.model.read(cx).tab_state(&self.channel.id, cx)
    }

    fn update_tabs<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut TabState) -> R) -> R {
        let id = self.channel.id.clone();
        let (result, opened, ids) = self.model.update(cx, |model, cx| {
            model.update_tab_state(&id, cx, |state| {
                let was = state.showing();
                let result = f(state);
                (result, !was && state.showing(), state.all_ids())
            })
        });
        if opened {
            self.opening += 1;
        }
        self.tab_views.retain(|id, _| ids.contains(id));
        cx.notify();
        result
    }

    /// A launcher's panel in the pane, which the toggle hides with it.
    fn side_drawer(&self) -> bool {
        self.drawer
            .as_ref()
            .is_some_and(|d| d.placement == channel_panels::Placement::Side)
    }

    /// The heading toggle: hides or shows the pane with what it holds.
    /// With nothing to show it is disabled.
    fn toggle_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.side_drawer();
        let showing = self.update_tabs(cx, |state| {
            state.toggle_pane(side);
            state.showing()
        });
        match self.tabs(cx).selected_tab() {
            Some(id) if showing => self.focus_tab(&id, window, cx),
            _ => self.focus_composer(window, cx),
        }
    }

    /// Choosing a tab resumes typing in its composer, where it has one.
    fn select_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.update_tabs(cx, |state| state.select(id));
        self.ensure_tab_views(window, cx);
        let panel = match self.tab_views.get(id) {
            Some(TabView::Thread { panel, .. }) => Some(panel.clone()),
            _ if id == channel_tabs::THREAD => self.thread.clone(),
            _ => None,
        };
        match panel {
            Some(panel) => ThreadPanel::focus_composer(&panel, window, cx),
            None => self.focus_tab(id, window, cx),
        }
    }

    fn close_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = id.to_owned();
        let remaining = self.update_tabs(cx, |state| {
            if state.panels.iter().any(|panel| panel.id() == id) {
                state.close_panel(&id);
            } else {
                state.close_tab(&id);
            }
            state.selected_tab()
        });
        match remaining {
            Some(next) => self.focus_tab(&next, window, cx),
            None => self.focus_composer(window, cx),
        }
    }

    fn close_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.thread = None;
        let remaining = self.update_tabs(cx, |state| {
            state.close_thread();
            state.selected_tab()
        });
        match remaining {
            Some(next) => self.focus_tab(&next, window, cx),
            None => self.focus_composer(window, cx),
        }
    }

    fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        let composer = self.page.read(cx).composer();
        composer.update(cx, |state, cx| state.focus(window, cx));
    }

    fn focus_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_tab_views(window, cx);
        if id == channel_tabs::THREAD {
            if let Some(thread) = &self.thread {
                ThreadPanel::focus(thread, window, cx);
            }
            return;
        }
        match self.tab_views.get(id) {
            Some(TabView::Conversation(page)) => {
                let composer = page.read(cx).composer();
                composer.update(cx, |state, cx| state.focus(window, cx));
            }
            Some(TabView::Thread { panel, .. }) => ThreadPanel::focus(panel, window, cx),
            _ => {}
        }
    }

    fn make_thread(
        &mut self,
        target: ThreadTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadPanel> {
        let weak = cx.entity().downgrade();
        let on_close: thread::OnClose = Arc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.close_thread(window, cx));
        });
        let (community, channel) = (self.community.clone(), self.channel.id.clone());
        let panel = cx.new(|cx| match target.exact {
            true => ThreadPanel::exact(community, channel, target.root, on_close, window, cx),
            false => ThreadPanel::new(community, channel, target.root, on_close, window, cx),
        });
        self.thread = Some(panel.clone());
        panel
    }

    fn open_thread(&mut self, target: ThreadTarget, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self.make_thread(target, window, cx);
        self.update_tabs(cx, |state| state.open_thread(target));
        ThreadPanel::focus(&panel, window, cx);
    }

    /// Makes a registered panel's view, opened from `channel` to show
    /// `target` and closed through `close`.
    #[allow(clippy::too_many_arguments)]
    fn panel_view(
        &self,
        panel: &ChannelPanel,
        channel: String,
        target: String,
        root: Option<String>,
        close: channel_panels::Close,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyView {
        (panel.build)(
            channel_panels::Opening {
                community: self.community.clone(),
                channel,
                target,
                close,
                thread: self.tabs(cx).thread.map(|thread| thread.root.to_hex()),
                root,
            },
            window,
            cx,
        )
    }

    fn toggle_launcher(
        &mut self,
        panel: ChannelPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = PanelTab::tool(panel.key, &self.channel.id);
        let state = self.tabs(cx);
        if state.panels.contains(&tab) {
            if state.showing() && state.selected_tab() == Some(tab.id()) {
                self.update_tabs(cx, |state| state.pane_open = false);
            } else {
                self.select_tab(&tab.id(), window, cx);
            }
            return;
        }
        if self.drawer.as_ref().is_some_and(|d| d.key == panel.key) {
            self.drawer = None;
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        let Some(launcher) = panel.launcher else {
            return;
        };
        let weak = cx.entity().downgrade();
        let key = panel.key;
        let close: channel_panels::Close = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if this.drawer.as_ref().is_some_and(|d| d.key == key) {
                    this.drawer = None;
                    this.focus_composer(window, cx);
                    cx.notify();
                }
            });
        });
        let channel = self.channel.id.clone();
        let target = launcher
            .target
            .map_or_else(|| channel.clone(), str::to_owned);
        let view = self.panel_view(&panel, channel, target, None, close, window, cx);
        self.update_tabs(cx, |state| state.pane_open = true);
        self.drawer = Some(Drawer {
            key,
            placement: launcher.placement,
            view,
        });
        cx.notify();
    }

    fn ensure_tab_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        channel_tab_views::ensure_views(self, window, cx);
    }

    fn tab_items(
        &mut self,
        state: &TabState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<TabItem> {
        self.ensure_tab_views(window, cx);
        let c = *cx.colors();
        let weak = cx.entity().downgrade();
        let closer = |id: String| -> Close {
            let weak = weak.clone();
            Rc::new(move |window, cx| {
                let id = id.clone();
                let _ = weak.update(cx, |this, cx| this.close_tab(&id, window, cx));
            })
        };
        let mut items = Vec::new();
        if state.thread.is_some()
            && let Some(thread) = self.thread.clone()
        {
            thread.update(cx, |thread, _| thread.show_header = false);
            let weak = cx.entity().downgrade();
            items.push(TabItem {
                id: channel_tabs::THREAD.into(),
                label: "Thread".into(),
                icon: IconName::ChatCircle,
                content: thread.into_any_element(),
                close: Rc::new(move |window, cx| {
                    let _ = weak.update(cx, |this, cx| this.close_thread(window, cx));
                }),
                drag: None,
            });
        }
        for tab in state.side_tabs() {
            let (label, icon): (SharedString, IconName) = match &tab.kind {
                TabKind::Conversation { channel } => match usable(&self.community, channel, cx) {
                    Some(summary) => (summary.name.clone().into(), tab_icon(&summary)),
                    None => ("Unavailable conversation".into(), IconName::Browser),
                },
                TabKind::Thread(thread) => {
                    let name = thread_channel(self, thread, cx).map(|summary| summary.name);
                    let content = root_content(self, thread, cx);
                    (
                        thread_label(content.as_deref(), name.as_deref()).into(),
                        IconName::ChatCircle,
                    )
                }
            };
            let content = match self.tab_views.get(&tab.id) {
                Some(TabView::Conversation(page)) => page.clone().into_any_element(),
                Some(TabView::Thread { panel, .. }) => panel.clone().into_any_element(),
                Some(TabView::Panel(view)) => view.clone().into_any_element(),
                Some(TabView::Settings { .. }) | None => div()
                    .px(rems(1.))
                    .py(rems(2.))
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("This conversation is no longer available.")
                    .into_any_element(),
            };
            items.push(TabItem {
                id: tab.id.clone().into(),
                label: label.clone(),
                icon,
                content,
                close: closer(tab.id.clone()),
                drag: tab.thread().map(|_| TabDrag {
                    id: tab.id.clone(),
                    label: label.clone(),
                    pane: TabPane::Side,
                }),
            });
        }
        for opening in &state.panels {
            let id = opening.id();
            let Some(panel) = channel_panels::get(opening.panel, cx) else {
                continue;
            };
            let content = match self.tab_views.get(&id) {
                Some(TabView::Panel(view)) => view.clone().into_any_element(),
                _ => div().into_any_element(),
            };
            items.push(TabItem {
                id: id.clone().into(),
                label: panel.title.into(),
                icon: panel.icon,
                content,
                close: closer(id),
                drag: None,
            });
        }
        items
    }

    /// `SessionHeading` with Me's actions.
    fn heading(
        &mut self,
        (showing, has_content): (bool, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let channel = self.channel.clone();
        let mut header = PanelHeader::new()
            .joined(true)
            .label(Some(channel_icon(&channel)), channel.name.clone());
        if channel.archived {
            header = header.action(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("Archived"),
            );
        } else {
            let launchers: Vec<AnyElement> = if channel.read_only {
                Vec::new()
            } else {
                channel_panels::launchers(cx)
                    .into_iter()
                    .map(|(panel, launcher)| {
                        let pressed = self.drawer.as_ref().is_some_and(|d| d.key == panel.key);
                        IconButton::new(
                            SharedString::from(format!("me-launch-{}", panel.key)),
                            launcher.icon,
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(launcher.label)
                        .selected(pressed)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_launcher(panel.clone(), window, cx)
                        }))
                        .into_any_element()
                    })
                    .collect()
            };
            let share_channel = channel.clone();
            header = header
                .action(self.page.read(cx).header_actions())
                .action(div().flex().gap(rems(0.375)).children(launchers))
                .action(
                    Button::new("me-share", "Share")
                        .size(ButtonSize::Sm)
                        .ghost()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let dialog = dialogs::Dialog::share(
                                this.community.clone(),
                                share_channel.clone(),
                                window,
                                cx,
                            );
                            this.dialog = Some(dialog);
                            cx.notify();
                        })),
                );
        }
        let menu_open = self.menu_open;
        let menu = menu_open.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.menu_open = false;
                    cx.notify();
                });
            });
            let (id, name) = (channel.id.clone(), channel.name.clone());
            let (settings_id, settings_name) = (id.clone(), name.clone());
            let content = Menu::new()
                .child(
                    MenuItem::new("me-session-rename", "Rename")
                        .icon(IconName::PencilSimple)
                        .icon_size(16.)
                        .disabled(channel.read_only)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.menu_open = false;
                            let dialog = dialogs::Dialog::rename(
                                this.community.clone(),
                                id.clone(),
                                name.clone(),
                                window,
                                cx,
                            );
                            this.dialog = Some(dialog);
                            cx.notify();
                        })),
                )
                .child(
                    MenuItem::new("me-session-settings", "Chat settings…")
                        .icon(IconName::Gear)
                        .icon_size(16.)
                        .disabled(channel.read_only)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.menu_open = false;
                            let dialog = dialogs::Dialog::session_settings(
                                this.community.clone(),
                                settings_id.clone(),
                                settings_name.clone(),
                                window,
                                cx,
                            );
                            this.dialog = Some(dialog);
                            cx.notify();
                        })),
                );
            overlay::dropdown(window, Align::End, content, dismiss)
        });
        header
            .action(
                div()
                    .relative()
                    .child(
                        IconButton::new("me-session-actions", IconName::DotsThree)
                            .size(IconButtonSize::Sm)
                            .selected(menu_open)
                            .when(!menu_open, |b| b.tooltip("Session actions"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.menu_open = true;
                                cx.notify();
                            })),
                    )
                    .children(menu),
            )
            .action(
                IconButton::new("me-toggle-tab-pane", IconName::SidebarRight)
                    .size(IconButtonSize::Sm)
                    .tooltip(if showing {
                        "Close tab pane"
                    } else {
                        "Open tab pane"
                    })
                    .selected(showing)
                    .disabled(!has_content)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_pane(window, cx))),
            )
            .into_any_element()
    }

    /// A dragged tab moved: Me's tabs reorder within the side pane.
    fn drag_moved(
        &mut self,
        drag: &TabDrag,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_cancelled {
            return;
        }
        let Some(start) = self.tab_geometry.borrow().start else {
            return;
        };
        if start.viewport != window.viewport_size() {
            self.drag_target = None;
            self.drag_cancelled = true;
            cx.notify();
            return;
        }
        let state = self.tabs(cx);
        let can_move = |pane: TabPane| pane == TabPane::Side;
        let strip = state.root_ids();
        let rules = Rules {
            order: vec![PaneOrder {
                pane: TabPane::Side,
                tabs: state.side_tabs().map(|tab| tab.id.as_str()).collect(),
                strip: strip.iter().map(String::as_str).collect(),
            }],
            can_move: &can_move,
            side_visible: true,
            closed_side_index: None,
        };
        let target = tab_drag::target(
            &self.tab_geometry.borrow(),
            drag,
            start.at,
            position,
            &rules,
            self.drag_target,
        );
        let header = self
            .tab_geometry
            .borrow()
            .headers
            .get(&tab_drag::PaneKey::Side)
            .copied();
        if header.is_some_and(|header| header.contains(&position)) {
            tab_drag::scroll_near_ends(&self.tab_strip, position);
        }
        if target != self.drag_target {
            self.drag_target = target;
            cx.notify();
        }
    }

    fn drag_dropped(&mut self, drag: &TabDrag, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.drag_target.take();
        let cancelled = std::mem::take(&mut self.drag_cancelled);
        TabGeometry::finish(&self.tab_geometry);
        cx.notify();
        let Some(target) = target.filter(|_| !cancelled) else {
            return;
        };
        let state = self.tabs(cx);
        let order: Vec<&str> = state.side_tabs().map(|tab| tab.id.as_str()).collect();
        if order.iter().position(|id| *id == drag.id) == Some(target.index) {
            return;
        }
        let id = drag.id.clone();
        self.update_tabs(cx, |state| state.move_tab(&id, target.pane, target.index));
        self.focus_tab(&id, window, cx);
    }

    /// Placement notices under the heading.
    fn placement(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = *cx.colors();
        let model = self.model.read(cx);
        let session = self.community.read(cx).session.clone();
        if let Some((text, retry)) =
            super::placement_retry("me-work-retry", &model.kit, session.me_kit())
        {
            return Some(notice(text, cx).child(retry).into_any_element());
        }
        if !model.placed(&self.channel.id) {
            let id = self.channel.id.clone();
            return Some(
                notice("This conversation is in Messages.", cx)
                    .child(Button::new("me-work-open", "Open in Messages").on_click(
                        move |_, window, cx| {
                            crate::ui::navigation::open_conversation(id.clone(), window, cx)
                        },
                    ))
                    .text_color(c.text_standard)
                    .into_any_element(),
            );
        }
        None
    }
}

fn notice(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(rems(0.75))
        .px(rems(1.5))
        .py(rems(0.5))
        .text_body_sm()
        .text_color(cx.colors().text_subtle)
        .child(text.into())
}

impl DialogHost for SessionWork {
    fn dialog(&mut self) -> &mut Option<dialogs::Dialog> {
        &mut self.dialog
    }
}

impl Render for SessionWork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let laid_out = TabGeometry::clear(&self.tab_geometry);
        if !cx.has_active_drag() {
            self.drag_target = None;
            self.drag_cancelled = false;
            TabGeometry::finish(&self.tab_geometry);
        }
        let state = self.tabs(cx);
        channel_tab_views::mount_composers(self, window, cx);
        // Me's own thread too, while its tab is not the one shown.
        if let Some(thread) = self.thread.clone() {
            thread.update(cx, |thread, cx| thread.mount_composer(window, cx));
        }
        let showing = state.showing();
        let has_content = state.has_panel() || self.side_drawer();
        let heading = self.heading((state.pane_open && has_content, has_content), window, cx);
        let placement = self.placement(cx);
        // The controls wait for placement and stop for archived chats.
        let disabled = self.channel.archived || !self.model.read(cx).placed(&self.channel.id);
        let controls = self.controls.clone();
        controls.update(cx, |controls, cx| controls.set_disabled(disabled, cx));
        let side = self
            .drawer
            .as_ref()
            .filter(|d| d.placement == channel_panels::Placement::Side)
            .map(|d| d.view.clone());
        let bottom = self
            .drawer
            .as_ref()
            .filter(|d| d.placement == channel_panels::Placement::Bottom)
            .map(|d| d.view.clone());
        let pane = showing.then(|| {
            let items = self.tab_items(&state, window, cx);
            let selected = state.selected_tab().unwrap_or_default();
            if self.revealed_tab.as_deref() != Some(selected.as_str()) {
                let bounds = laid_out.get(&selected).copied();
                match panels::reveal_tab(&self.tab_strip, bounds) {
                    false => window.request_animation_frame(),
                    true => self.revealed_tab = Some(selected.clone()),
                }
            }
            let weak = cx.entity().downgrade();
            panels::tabs(
                items,
                &selected,
                &self.tab_strip,
                Rc::new(move |id, window, cx| {
                    let _ = weak.update(cx, |this, cx| this.select_tab(&id, window, cx));
                }),
                Some(&self.tab_geometry),
                cx,
            )
        });
        let side = side
            .filter(|_| state.pane_open)
            .map(IntoElement::into_any_element);
        let rows: Vec<AnyElement> = [pane, side].into_iter().flatten().collect();
        let border = cx.colors().border_standard;
        let dock = (!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .size_full()
                .children(rows.into_iter().enumerate().map(|(index, row)| {
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .when(index > 0, |this| this.border_t_1().border_color(border))
                        .child(row)
                }))
                .into_any_element()
        });
        let conversation = div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .child(heading)
            .children(placement)
            .child(div().flex().flex_1().min_h_0().child(self.page.clone()))
            .when_some(bottom, |this, view| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .h(relative(0.42))
                        .min_h(px(180.))
                        .max_h(relative(0.65))
                        .border_t_1()
                        .border_color(border)
                        .overflow_hidden()
                        .child(view),
                )
            })
            .into_any_element();
        let weak = cx.entity().downgrade();
        let available = f32::from(window.viewport_size().width);
        let board = Dock {
            id: "me-board".into(),
            split: Split::BOARD,
            preferred: self.width,
            window: available,
            available: available * 0.75,
            opening: self.opening,
            on_resize: Rc::new(move |width, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.width = width;
                    cx.notify();
                });
            }),
            on_commit: Rc::new(|_, _| {}),
        }
        .render(conversation, dock, cx);
        let (moved, dropped, escape) = (
            cx.entity().downgrade(),
            cx.entity().downgrade(),
            cx.entity().downgrade(),
        );
        let feedback = cx
            .has_active_drag()
            .then(|| tab_drag::feedback(self.drag_target, cx))
            .flatten();
        div()
            .id("me-session")
            .size_full()
            .flex()
            .on_drag_move::<TabDrag>(move |event, window, cx| {
                let drag = event.drag(cx).clone();
                let position = event.event.position;
                let _ = moved.update(cx, |this, cx| this.drag_moved(&drag, position, window, cx));
            })
            .on_drop::<TabDrag>(move |drag, window, cx| {
                let drag = drag.clone();
                let _ = dropped.update(cx, |this, cx| this.drag_dropped(&drag, window, cx));
            })
            .capture_key_down(move |event, _, cx| {
                if event.keystroke.key == "escape" && cx.has_active_drag() {
                    cx.stop_propagation();
                    let _ = escape.update(cx, |this, cx| {
                        this.drag_target = None;
                        this.drag_cancelled = true;
                        cx.notify();
                    });
                }
            })
            .child(board)
            .children(feedback)
            .children(
                self.dialog
                    .as_ref()
                    .map(|dialog| dialog.overlay(window, cx)),
            )
    }
}
