//! The Me page (`bundled/me`, `bundled/sessions/SessionsPage.tsx`): the
//! viewer's personal conversations with agents.
//!
//! Me lists the conversations placed in it (the Me kit record's
//! `channels`) in the viewer's Me groups. Choosing one shows it on its own
//! board; "New chat" starts one, which creates a private work
//! session, places it in Me and sends the first message. Which of these
//! shows is Me's selection, kept per community by [`MeModel`], which the
//! page and its sidebar share.

mod board;
mod controls;
pub mod dialogs;
mod new_session;
pub mod pending;
pub mod recipients;
mod rename;
mod sections;
pub mod settings;
mod share;
mod workspace;

use super::channel_tabs::TabState;
use super::pages::{self, Navigation, Page, PageRoutes, PageSpec, Placement};
use crate::community::{AppState, Community};
use crate::mirror::mirror;
pub(crate) use board::SessionWork;
use buzz_relay_client::session::{MeKit, MeKitSnapshot};
use buzz_relay_client::sidebar_store::SidebarState;
use buzz_relay_client::{ChannelSummary, RosterStatus, SessionStatus};
use buzz_ui::{ActiveTheme, Button, Icon, IconName, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use new_session::NewSession;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// buzz-app's legacy Sessions page, which now opens Me.
pub const SESSIONS: Page = Page::new("buzz.sessions/sessions");

pub fn register(cx: &mut App) {
    // Me placement is read with the channel list even before Me opens:
    // placed conversations leave the Messages sidebar.
    MeModel::global(cx);
    pages::register(
        PageSpec {
            page: Page::ME,
            label: "Me",
            // buzz-app gives Me no bundled icon, so search shows the
            // generic page glyph.
            icon: IconName::Browser,
            placement: Placement::Topbar,
            navigation: Navigation::Own(|window, cx| {
                cx.new(|cx| sections::MeSidebar::new(window, cx)).into()
            }),
            collapsible: true,
            badge: None,
            build: pages::build(|window, cx| cx.new(|cx| MePage::new(window, cx)).into()),
        },
        cx,
    );
    // Route v1 (`isMeRoute`): a new conversation, in a group, or one by id.
    pages::register_routes(
        Page::ME,
        PageRoutes {
            version: 1,
            validate: |params| Selection::parse(params).is_some(),
            open: |_, params, _, cx| {
                if let Some(selection) = params.and_then(Selection::parse) {
                    MeModel::global(cx).update(cx, |me, cx| me.show(selection, cx));
                }
            },
        },
        cx,
    );
    pages::register(
        PageSpec {
            page: SESSIONS,
            label: "Sessions (now in Me)",
            icon: IconName::Browser,
            placement: Placement::Hidden,
            navigation: Navigation::Channels,
            collapsible: false,
            badge: None,
            build: pages::build(|_, cx| cx.new(|_| LegacySessions { opened: false }).into()),
        },
        cx,
    );
}

/// What Me shows (`me/routes.ts`): a new conversation, optionally in a
/// group (`new`, `new:<section>`), or a conversation by id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    #[default]
    New,
    NewIn(String),
    Session(String),
}

impl Selection {
    /// Route params as `isMeRoute` takes them: `"new"`, `"new:<section>"`,
    /// or a conversation's lowercase UUID.
    pub fn parse(params: &serde_json::Value) -> Option<Self> {
        let route = params.as_str()?;
        let uuid = route.len() == 36
            && route.bytes().enumerate().all(|(at, byte)| match at {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
            });
        match route {
            "new" => Some(Self::New),
            _ if uuid => Some(Self::Session(route.to_owned())),
            _ => route
                .strip_prefix("new:")
                .filter(|_| route.len() > 4 && route.len() <= 260)
                .map(|section| Self::NewIn(section.to_owned())),
        }
    }

    /// The route params for this selection (`meTarget`).
    pub fn params(&self) -> String {
        match self {
            Self::New => "new".to_owned(),
            Self::NewIn(section) => format!("new:{section}"),
            Self::Session(id) => id.clone(),
        }
    }

    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Session(id) => Some(id),
            _ => None,
        }
    }

    pub fn section(&self) -> Option<&str> {
        match self {
            Self::NewIn(section) => Some(section),
            _ => None,
        }
    }
}

/// Me's state for the selected community, shared by the page and its
/// sidebar.
pub struct MeModel {
    pub community: Option<Entity<Community>>,
    /// The selection per community origin.
    selections: HashMap<String, Selection>,
    /// Bumped by every navigation inside Me, so a view started for an
    /// earlier visit can tell (`navigation.signal`).
    pub visit: u64,
    /// Whether the view the visit opens takes the focus. Opening Me from
    /// its topbar tab leaves the focus where it is (`focus: "preserve"`);
    /// every navigation inside Me focuses its content again.
    pub autofocus: bool,
    pub kit: Arc<MeKitSnapshot>,
    pub me: Arc<SidebarState>,
    /// The conversations placed in Me, read from `kit` as it changes.
    placed: HashSet<String>,
    /// Each Me conversation's tabs, by community and channel, apart from
    /// its tabs in Messages (`useChannelTabState(..., "me")`).
    tabs: HashMap<(String, String), TabState>,
    _tasks: Vec<Task<()>>,
    _community: Option<Subscription>,
    _app: Subscription,
}

struct MeHub(Entity<MeModel>);

impl Global for MeHub {}

impl MeModel {
    /// The one model, made on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(hub) = cx.try_global::<MeHub>() {
            return hub.0.clone();
        }
        let model = cx.new(|cx| {
            let app = cx.observe_global::<AppState>(|this: &mut Self, cx| this.sync(cx));
            let mut model = Self {
                community: None,
                selections: HashMap::new(),
                visit: 0,
                autofocus: true,
                kit: Arc::default(),
                me: Arc::default(),
                placed: HashSet::new(),
                tabs: HashMap::new(),
                _tasks: Vec::new(),
                _community: None,
                _app: app,
            };
            model.sync(cx);
            model
        });
        cx.set_global(MeHub(model.clone()));
        model
    }

    /// Follows the selected community: its catalog and Me groups.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let current = AppState::current(cx);
        if current.as_ref().map(Entity::entity_id) == self.community.as_ref().map(Entity::entity_id)
        {
            return;
        }
        self.community = current.clone();
        self._tasks.clear();
        self._community = None;
        self.kit = Arc::default();
        self.me = Arc::default();
        self.placed.clear();
        if let Some(community) = current {
            let kit = community.read(cx).session.me_kit().clone();
            self._tasks
                .push(mirror(kit.watch(), cx, |this: &mut Self, value| {
                    this.placed = value.me_channels().into_iter().collect();
                    this.kit = value;
                }));
            self._tasks
                .push(mirror(kit.watch_me(), cx, |this: &mut Self, value| {
                    this.me = value
                }));
            self._community = Some(cx.observe(&community, |this, community, cx| {
                this.ensure(&community, cx);
                cx.notify();
            }));
            self.ensure(&community, cx);
        }
        cx.notify();
    }

    /// Placement and groups read once the channel list is ready
    /// (`useMePlacement`, `SessionSections`).
    fn ensure(&self, community: &Entity<Community>, cx: &App) {
        let community = community.read(cx);
        if community.snapshot.status == SessionStatus::Ready
            && community.snapshot.roster == RosterStatus::Ready
        {
            let kit = community.session.me_kit();
            kit.ensure();
            kit.me_ensure();
        }
    }

    fn scope(&self, cx: &App) -> Option<String> {
        Some(self.community.as_ref()?.read(cx).origin.to_string())
    }

    pub fn selection(&self, cx: &App) -> Selection {
        self.scope(cx)
            .and_then(|scope| self.selections.get(&scope).cloned())
            .unwrap_or_default()
    }

    /// Goes to `selection` in Me (`navigator.open(meTarget(...))`): a
    /// visit of its own while Me shows, so Back returns to the one before.
    pub fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        let params = serde_json::Value::String(selection.params());
        self.show(selection, cx);
        crate::ui::navigation::with_workspace(cx, move |workspace, _, cx| {
            workspace.record_route(Page::ME, params, cx)
        });
    }

    /// Shows `selection`, as its visit opens.
    pub fn show(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if let Some(scope) = self.scope(cx) {
            self.selections.insert(scope, selection);
        }
        self.visit += 1;
        self.autofocus = true;
        cx.notify();
    }

    /// The conversations placed in Me, newest activity first
    /// (`PersonalHistory`).
    pub fn conversations(&self, cx: &App) -> Vec<ChannelSummary> {
        let Some(community) = &self.community else {
            return Vec::new();
        };
        let mut list: Vec<ChannelSummary> = community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .filter(|channel| !channel.archived && self.placed.contains(&channel.id))
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            let time = |c: &ChannelSummary| c.last_activity_at.or(c.updated_at).unwrap_or(0);
            time(b).cmp(&time(a)).then_with(|| a.id.cmp(&b.id))
        });
        list
    }

    pub fn tab_state(&self, channel: &str, cx: &App) -> TabState {
        self.scope(cx)
            .and_then(|scope| self.tabs.get(&(scope, channel.to_owned())).cloned())
            .unwrap_or_default()
    }

    /// Panel `key` was taken away: no saved Me conversation keeps a tab
    /// for it.
    pub fn withdraw_panel(&mut self, key: &str, cx: &mut Context<Self>) {
        let prefix = format!("{key}:");
        for state in self.tabs.values_mut() {
            state.panels.retain(|tab| tab.panel != key);
            if state.selected.starts_with(&prefix) {
                state.selected = String::new();
            }
        }
        cx.notify();
    }

    pub fn update_tab_state<R>(
        &mut self,
        channel: &str,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut TabState) -> R,
    ) -> R {
        let scope = self.scope(cx).unwrap_or_default();
        let state = self.tabs.entry((scope, channel.to_owned())).or_default();
        f(state)
    }

    /// Whether the conversation is placed in Me (`placement.has`).
    pub fn placed(&self, id: &str) -> bool {
        self.kit.ready() && self.placed.contains(id)
    }
}

/// Opens Me in place of the legacy Sessions page ("Opening Me…").
struct LegacySessions {
    opened: bool,
}

impl Render for LegacySessions {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !std::mem::replace(&mut self.opened, true) {
            super::navigation::with_workspace(cx, |workspace, window, cx| {
                workspace.select_page(Page::ME, window, cx)
            });
        }
        div()
            .p(rems(1.5))
            .text_body_sm()
            .text_color(cx.colors().text_subtle)
            .child("Opening Me…")
    }
}

/// Me's main column (`SessionsPage`).
pub struct MePage {
    model: Entity<MeModel>,
    /// The new-conversation view, by community and draft key.
    new: Option<(EntityId, String, Entity<NewSession>)>,
    /// The open conversation, by community, id and visit.
    work: Option<(EntityId, String, u64, Entity<SessionWork>)>,
    /// The draft whose saved start creates the selected conversation, by
    /// community, conversation and [`pending::generation`].
    pending: Option<(EntityId, String, u64, Option<String>)>,
    _model: Subscription,
}

impl MePage {
    /// The conversation open here, if any.
    pub(crate) fn work(&self) -> Option<Entity<SessionWork>> {
        self.work.as_ref().map(|(_, _, _, work)| work.clone())
    }

    fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = MeModel::global(cx);
        let subscription = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            new: None,
            work: None,
            pending: None,
            _model: subscription,
        }
    }

    fn empty(&self, community: Option<&Entity<Community>>, cx: &App) -> AnyElement {
        let c = cx.colors();
        let (status, retry) = match community.map(|c| c.read(cx)) {
            Some(community) => (
                Some(community.snapshot.status.clone()),
                Some(community.session.clone()),
            ),
            None => (None, None),
        };
        let text = match status {
            Some(SessionStatus::Connecting) => "Connecting to your community…",
            _ => "Connect to a community to work with your agents.",
        };
        let failure = match status {
            Some(SessionStatus::Failed(error)) => Some(if error.is_empty() {
                "The connection failed.".to_owned()
            } else {
                error
            }),
            _ => None,
        };
        centered()
            .child(Icon::new(IconName::ChatCircle).size(28.))
            .child(div().text_heading().child("Me"))
            .child(subtle(text, cx))
            .when_some(failure, |this, error| {
                this.child(
                    div()
                        .max_w(rems(26.25))
                        .text_body()
                        .text_color(c.text_danger)
                        .child(error),
                )
                .child(
                    Button::new("me-retry-connection", "Retry connection").on_click(
                        move |_, _, _| {
                            if let Some(session) = &retry {
                                session.retry();
                            }
                        },
                    ),
                )
            })
            .into_any_element()
    }

    /// [`pending::draft_key_for`], read again only when the selection or
    /// a saved start changes.
    fn draft_key_for(
        &mut self,
        community: &Entity<Community>,
        id: &str,
        cx: &App,
    ) -> Option<String> {
        let scope = community.entity_id();
        let generation = pending::generation();
        if let Some((owner, open, at, key)) = &self.pending
            && *owner == scope
            && open == id
            && *at == generation
        {
            return key.clone();
        }
        let key = pending::draft_key_for(community, id, cx);
        self.pending = Some((scope, id.to_owned(), generation, key.clone()));
        key
    }

    /// A conversation that is not (or not yet) on Me's list.
    fn missing(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        let Some(community) = model.community.clone() else {
            return div().into_any_element();
        };
        let snapshot = community.read(cx).snapshot.clone();
        let session = community.read(cx).session.clone();
        if let Some((text, retry)) =
            placement_retry("me-retry-placement", &model.kit, session.me_kit())
        {
            return centered()
                .child(subtle(text, cx))
                .child(retry)
                .into_any_element();
        }
        if snapshot.roster == RosterStatus::Loading {
            return centered()
                .child(subtle("Loading your conversation…", cx))
                .into_any_element();
        }
        let listed = snapshot.channels.iter().any(|channel| channel.id == id);
        let open = id.to_owned();
        centered()
            .child(subtle(
                "This conversation isn’t in your Me workspace. Open it in Messages.",
                cx,
            ))
            .when(listed, |this| {
                this.child(
                    Button::new("me-open-in-messages", "Open in Messages").on_click(
                        move |_, window, cx| {
                            super::navigation::open_conversation(open.clone(), window, cx)
                        },
                    ),
                )
            })
            .child(
                Button::new("me-retry-list", "Retry")
                    .on_click(move |_, _, _| session.refresh_channels()),
            )
            .into_any_element()
    }
}

/// While Me's placement is not read: what to say (the error, or that it
/// is loading) and a "Retry Me" button that reads it again.
fn placement_retry(
    id: &'static str,
    kit: &MeKitSnapshot,
    me_kit: &MeKit,
) -> Option<(String, Button)> {
    if kit.ready() {
        return None;
    }
    let text = kit
        .error()
        .map(str::to_owned)
        .unwrap_or_else(|| "Loading Me placement…".into());
    let me_kit = me_kit.clone();
    let retry = Button::new(id, "Retry Me").on_click(move |_, _, _| drop(me_kit.refresh()));
    Some((text, retry))
}

/// A centered column of status text (`.empty`).
fn centered() -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(rems(0.75))
        .p(rems(2.))
        .text_center()
}

fn subtle(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .max_w(rems(26.25))
        .text_body()
        .text_color(cx.colors().text_subtle)
        .child(text.into())
}

impl Render for MePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let community = model.community.clone();
        let ready = community
            .as_ref()
            .is_some_and(|c| c.read(cx).snapshot.status == SessionStatus::Ready);
        let content = if !ready {
            self.new = None;
            self.work = None;
            self.empty(community.as_ref(), cx)
        } else {
            let community = community.expect("ready implies a community");
            let selection = model.selection(cx);
            let visit = model.visit;
            let focus = model.autofocus;
            let pending = selection
                .session()
                .and_then(|id| self.draft_key_for(&community, id, cx));
            match (&selection, pending) {
                (Selection::Session(id), None) => {
                    self.new = None;
                    let channel = community
                        .read(cx)
                        .snapshot
                        .channels
                        .iter()
                        .find(|channel| channel.id == *id)
                        .cloned();
                    match channel {
                        Some(channel) => {
                            let scope = community.entity_id();
                            let current = self
                                .work
                                .as_ref()
                                .filter(|(owner, open, at, _)| {
                                    *owner == scope && *open == channel.id && *at == visit
                                })
                                .map(|(_, _, _, view)| view.clone());
                            let view = current.unwrap_or_else(|| {
                                let view = cx.new(|cx| {
                                    SessionWork::new(
                                        community.clone(),
                                        channel.clone(),
                                        focus,
                                        window,
                                        cx,
                                    )
                                });
                                self.work = Some((scope, channel.id.clone(), visit, view.clone()));
                                view
                            });
                            view.into_any_element()
                        }
                        None => {
                            self.work = None;
                            let id = id.clone();
                            self.missing(&id, cx)
                        }
                    }
                }
                (_, pending) => {
                    self.work = None;
                    let key = pending.unwrap_or_else(|| pending::draft_key(selection.section()));
                    let scope = community.entity_id();
                    let current = self
                        .new
                        .as_ref()
                        .filter(|(owner, open, _)| *owner == scope && *open == key)
                        .map(|(_, _, view)| view.clone());
                    let view = current.unwrap_or_else(|| {
                        let section = pending::section_of(&key);
                        let view = cx.new(|cx| {
                            NewSession::new(
                                community.clone(),
                                key.clone(),
                                section,
                                focus,
                                window,
                                cx,
                            )
                        });
                        self.new = Some((scope, key, view.clone()));
                        view
                    });
                    view.into_any_element()
                }
            }
        };
        div()
            .id("me-page")
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .text_color(cx.colors().text_standard)
            .child(content)
    }
}
