//! The Agents page (buzz-app's `AgentsPage.tsx`), in three tabs: Your
//! agents, Import and Browse.
//! - Your agents: every agent as a card. Selecting one (click, or arrow
//!   keys) opens its peek beside the grid without leaving it; Enter or a
//!   double-click builds: the identity rail on the left, the type's tabs
//!   and the app's Attention tab on the right. The New agent tile opens
//!   the create dialog. Agents the owner's agent list (kind 30177) names
//!   that run on another device show too, read-only.
//! - Import: identities earlier agent systems saved on the hosts.
//! - Browse: agents shared in the community.
//!
//! The header's More menu opens Provider settings, each agent type's
//! readiness on this device with its provider dialog, and Teams in the
//! side panel. The page's routes open a provider dialog or an agent.

use super::attention::summary as attention_summary;
use super::bring_over::{BringOver, BringOverEvent};
use super::browse::{AgentBrowse, BrowseEvent};
use super::card::{self, AgentCard};
use super::create::{CreateAgent, CreateEvent, type_icon};
use super::routes;
use super::teams::{CloseTeams, TeamsPanel};
use super::{AgentDelete, AgentTabs, Removed, empty_state, problem};
use crate::agents2::listings::Elsewhere;
use crate::agents2::{
    Agent, AgentChange, Agents2, CloseProvider, ProviderChanges, Readiness, RegisteredType, Status,
    type_of, types,
};
use crate::ui::navigation::{self, Place, Target};
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::pages::Page;
use crate::ui::settings::avatar::{
    AvatarDismissed, AvatarDone, AvatarEditor, Preview, popover as avatar_popover,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, Colors, Dialog, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, PanelHeader, Spinner, Tab, TabVariant, Tabs, TextField, TypeStyled,
    tokens::{duration, easing, radius},
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::nips::nip19::ToBech32;
use std::rc::Rc;

/// Below this viewport width the side panel narrows and Build stacks.
const NARROW_REM: f32 = 52.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabId {
    Yours,
    Import,
    Browse,
}

const TABS: [(TabId, &str); 3] = [
    (TabId::Yours, "Your agents"),
    (TabId::Import, "Import"),
    (TabId::Browse, "Browse"),
];

#[derive(Clone, Debug, PartialEq)]
enum Frame {
    Browse {
        selected: Option<String>,
        teams: bool,
    },
    Build {
        pubkey: String,
    },
}

/// `npub…` and the key's last `suffix` characters (`formatPublicKey`).
fn short_key(pubkey: &str, suffix: usize) -> String {
    nostr::prelude::PublicKey::from_hex(pubkey)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .map(|npub| {
            let length = suffix.clamp(3, npub.len() - 5);
            format!("npub…{}", &npub[npub.len() - length..])
        })
        .unwrap_or_default()
}

/// The type's one-line summary of an agent; its title when it has none.
fn summary_of(agent: &Agent, kind: Option<&RegisteredType>) -> String {
    match kind {
        Some(kind) => kind.summary_of(agent),
        None => "Type unavailable".into(),
    }
}

/// One card of Your agents: an agent here, or one on another device.
#[derive(Clone)]
enum Shown {
    Here(Agent),
    Elsewhere(Elsewhere),
}

impl Shown {
    fn pubkey(&self) -> &str {
        match self {
            Shown::Here(agent) => &agent.pubkey,
            Shown::Elsewhere(agent) => &agent.pubkey,
        }
    }
}

/// The selected community's agents here, then those on the owner's other
/// devices.
fn shown(cx: &App) -> Vec<Shown> {
    let Some(service) = Agents2::global(cx) else {
        return Vec::new();
    };
    let service = service.read(cx);
    service
        .agents()
        .iter()
        .cloned()
        .map(Shown::Here)
        .chain(service.elsewhere(cx).into_iter().map(Shown::Elsewhere))
        .collect()
}

/// The selected community's origin, as navigation names it.
fn community(cx: &App) -> Option<String> {
    crate::community::AppState::current(cx).map(|community| community.read(cx).origin.to_string())
}

/// Closes the provider dialog: the page's start replaces its route.
fn close_provider(cx: &mut App) {
    let target = Target {
        community: community(cx),
        place: Place::Page(Page::AGENTS),
    };
    navigation::with_workspace(cx, move |workspace, window, cx| {
        workspace.navigate_replacing(target, window, cx)
    });
}

/// The route that opens an agent's Build view, for other pages.
pub(crate) fn agent_target(pubkey: &str, cx: &App) -> Target {
    Target {
        community: community(cx),
        place: Place::Route {
            page: Page::AGENTS,
            params: routes::agent_params(pubkey),
            version: crate::ui::pages::routes(Page::AGENTS, cx).map(|routes| routes.version),
        },
    }
}

/// The next selection for a key in the listbox: arrows move by one or by
/// a row, Home and End go to the ends. `None` when the key does nothing.
pub(crate) fn step(key: &str, from: Option<usize>, count: usize, columns: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let last = count as isize - 1;
    let columns = columns.max(1) as isize;
    let delta = match key {
        "right" => 1,
        "left" => -1,
        "down" => columns,
        "up" => -columns,
        "home" => return Some(0),
        "end" => return Some(last as usize),
        _ => return None,
    };
    let Some(from) = from else {
        return Some(0);
    };
    let from = from as isize;
    let next = from + delta;
    Some(if next < 0 || next > last {
        if delta.abs() == 1 {
            next.clamp(0, last) as usize
        } else {
            from as usize
        }
    } else {
        next as usize
    })
}

pub struct Agents2Page {
    tab: TabId,
    frame: Frame,
    grid: FocusHandle,
    create: Option<Entity<CreateAgent>>,
    import: Option<Entity<BringOver>>,
    browse: Option<Entity<AgentBrowse>>,
    teams: Option<Entity<TeamsPanel>>,
    #[cfg(feature = "voltage-sandbox")]
    migration: Option<Entity<super::remote_migration::RemoteMigration>>,
    build: Option<Entity<Build>>,
    more_open: bool,
    /// The Provider settings submenu is open.
    providers_open: bool,
    /// The provider dialog the route opened: its type's key, and its body.
    provider: Option<(SharedString, Option<AnyView>)>,
    /// Counts side panel openings, so each one animates in.
    side_opened: usize,
    side_key: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Agents2Page {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![
            cx.observe_global::<crate::agents2::registry::AgentTypes>(|_, cx| cx.notify()),
            cx.observe_global::<ProviderChanges>(|_, cx| cx.notify()),
        ];
        if let Some(service) = Agents2::global(cx) {
            subscriptions.push(cx.observe(&service, |_, _, cx| cx.notify()));
        }
        Self {
            tab: TabId::Yours,
            frame: Frame::Browse {
                selected: None,
                teams: false,
            },
            grid: cx.focus_handle(),
            create: None,
            import: None,
            browse: None,
            teams: None,
            #[cfg(feature = "voltage-sandbox")]
            migration: None,
            build: None,
            more_open: false,
            providers_open: false,
            provider: None,
            side_opened: 0,
            side_key: None,
            _subscriptions: subscriptions,
        }
    }

    /// Deletes a hosted agent through its provider.
    fn remove_hosted(&mut self, pubkey: String, cx: &mut Context<Self>) {
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        let task = service.update(cx, |service, cx| service.remove_hosted(&pubkey, cx));
        cx.spawn(async move |this, cx| {
            let removed = task.await;
            let _ = this.update(cx, |this, cx| match removed {
                Ok(()) => this.browse(None, cx),
                Err(error) => crate::ui::toasts::show(
                    crate::ui::toasts::Notice::error("hosted-delete", "Could not delete the agent")
                        .description(error),
                    cx,
                ),
            });
        })
        .detach();
    }

    fn browse(&mut self, selected: Option<String>, cx: &mut Context<Self>) {
        self.frame = Frame::Browse {
            selected,
            teams: false,
        };
        self.teams = None;
        self.build = None;
        cx.notify();
    }

    /// Opens an agent's Build view; a just-made agent may not be shown
    /// yet, and one on another device has none.
    fn build(&mut self, pubkey: String, window: &mut Window, cx: &mut Context<Self>) {
        if shown(cx)
            .iter()
            .any(|agent| matches!(agent, Shown::Elsewhere(a) if a.pubkey == pubkey))
        {
            return;
        }
        let view = cx.new(|cx| Build::new(pubkey.clone(), window, cx));
        cx.subscribe(&view, |this, _, _: &Removed, cx| this.browse(None, cx))
            .detach();
        self.build = Some(view);
        self.teams = None;
        self.frame = Frame::Build { pubkey };
        cx.notify();
    }

    /// Opens the agent at `pubkey`, as the agent route does.
    pub fn open_agent(&mut self, pubkey: String, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = TabId::Yours;
        self.create = None;
        self.build(pubkey, window, cx);
    }

    /// Shows a route's place (`navigation.target.route`); `None` is the
    /// page's start, which closes a provider dialog.
    pub fn open_route(
        &mut self,
        params: Option<&serde_json::Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(params) = params else {
            self.provider = None;
            return cx.notify();
        };
        if let Some(pubkey) = routes::agent_route(params) {
            self.provider = None;
            self.open_agent(pubkey.to_owned(), window, cx);
        } else if let Some(key) = routes::provider_route(params) {
            let key = SharedString::from(key.to_owned());
            if self.provider.as_ref().map(|(open, _)| open) == Some(&key) {
                return;
            }
            let close: CloseProvider = Rc::new(|_, cx| close_provider(cx));
            let body = type_of(&key, cx)
                .and_then(|kind| kind.kind.provider(cx))
                .map(|provider| provider.dialog_body(close, window, cx));
            self.provider = Some((key, body));
            cx.notify();
        }
    }

    /// Opens a type's provider dialog as a new visit.
    fn select_provider(&mut self, key: SharedString, cx: &mut Context<Self>) {
        self.more_open = false;
        self.providers_open = false;
        cx.notify();
        let target = Target {
            community: community(cx),
            place: Place::Route {
                page: Page::AGENTS,
                params: routes::provider_params(&key),
                version: crate::ui::pages::routes(Page::AGENTS, cx).map(|routes| routes.version),
            },
        };
        navigation::with_workspace(cx, move |workspace, window, cx| {
            workspace.navigate(target, window, cx)
        });
    }

    /// A row of the Provider settings submenu: the type's mark, a spinner
    /// while it checks, and what it needs.
    fn provider_row(
        &self,
        index: usize,
        kind: &RegisteredType,
        cx: &mut Context<Self>,
    ) -> MenuItem {
        let title = kind.kind.title();
        let id = ("agents-provider", index);
        let Some(provider) = kind.kind.provider(cx) else {
            return MenuItem::new(id, title)
                .leading(type_icon(Some(kind), 17., cx))
                .disabled(true);
        };
        let snapshot = provider.snapshot(cx);
        let checking = snapshot.readiness == Readiness::Checking;
        let label: SharedString = match snapshot.readiness {
            Readiness::Attention => snapshot
                .attention_label
                .unwrap_or_else(|| format!("Set up {title}"))
                .into(),
            _ => title,
        };
        let leading = if checking {
            Spinner::new().size(17.).into_any_element()
        } else {
            type_icon(Some(kind), 17., cx)
        };
        let key = kind.key.clone();
        MenuItem::new(id, label)
            .leading(leading)
            .disabled(checking)
            .on_click(cx.listener(move |this, _, _, cx| {
                // Attention handled directly, such as an install, skips
                // the dialog.
                let provider = type_of(&key, cx).and_then(|kind| kind.kind.provider(cx));
                if let Some(provider) = provider
                    && provider.snapshot(cx).readiness == Readiness::Attention
                    && let Some(action) = provider.attention_action(cx)
                {
                    this.more_open = false;
                    this.providers_open = false;
                    cx.notify();
                    return action(cx);
                }
                this.select_provider(key.clone(), cx);
            }))
    }

    /// The open provider dialog: the type's mark and title over its
    /// provider's body. Outside clicks close it unless its work blocks.
    fn provider_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (key, body) = self.provider.as_ref()?;
        let body = body.clone()?;
        let kind = type_of(key, cx)?;
        let blocking = kind.kind.provider(cx)?.snapshot(cx).blocking;
        let dialog = Dialog::new("agents-provider-dialog", kind.kind.title())
            .title_leading(type_icon(Some(&kind), 22., cx))
            .compact_header_gap()
            .when(!blocking, |dialog| {
                dialog.on_close(|_, _, cx| close_provider(cx))
            })
            .child(body);
        let dismiss: Option<Dismiss> = (!blocking)
            .then(|| Rc::new(|_: &mut Window, cx: &mut App| close_provider(cx)) as Dismiss);
        Some(overlay::modal(window, dialog, dismiss).into_any_element())
    }

    fn new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| CreateAgent::new(window, cx));
        cx.subscribe_in(
            &dialog,
            window,
            |this, _, event: &CreateEvent, window, cx| match event {
                CreateEvent::Close => {
                    this.create = None;
                    cx.notify();
                }
                CreateEvent::Created(pubkey) => {
                    this.create = None;
                    this.tab = TabId::Yours;
                    this.build(pubkey.clone(), window, cx);
                }
            },
        )
        .detach();
        self.create = Some(dialog);
        cx.notify();
    }

    #[cfg(feature = "voltage-sandbox")]
    fn open_migration(&mut self, team: Option<String>, cx: &mut Context<Self>) {
        let panel = cx.new(|cx| super::remote_migration::RemoteMigration::new(team, cx));
        cx.subscribe(
            &panel,
            |this, _, _: &super::remote_migration::CloseMigration, cx| {
                this.migration = None;
                cx.notify();
            },
        )
        .detach();
        self.more_open = false;
        self.migration = Some(panel);
        cx.notify();
    }

    fn open_teams(&mut self, cx: &mut Context<Self>) {
        self.more_open = false;
        let panel = cx.new(TeamsPanel::new);
        cx.subscribe(&panel, |this, _, _: &CloseTeams, cx| this.browse(None, cx))
            .detach();
        #[cfg(feature = "voltage-sandbox")]
        cx.subscribe(&panel, |this, _, event: &super::teams::MoveTeam, cx| {
            this.open_migration(Some(event.0.clone()), cx);
        })
        .detach();
        self.teams = Some(panel);
        self.frame = Frame::Browse {
            selected: None,
            teams: true,
        };
        cx.notify();
    }

    fn set_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == TabId::Browse && self.browse.is_none() {
            let view = cx.new(|cx| AgentBrowse::new(window, cx));
            cx.subscribe_in(
                &view,
                window,
                |this, _, event: &BrowseEvent, window, cx| match event {
                    BrowseEvent::Imported => this.set_tab(TabId::Yours, window, cx),
                },
            )
            .detach();
            self.browse = Some(view);
        }
        if tab == TabId::Import && self.import.is_none() {
            let view = cx.new(BringOver::new);
            cx.subscribe_in(
                &view,
                window,
                |this, _, event: &BringOverEvent, window, cx| match event {
                    BringOverEvent::Imported => this.set_tab(TabId::Yours, window, cx),
                },
            )
            .detach();
            self.import = Some(view);
        }
        if tab != TabId::Import {
            // Looks again the next time Import opens.
            self.import = None;
        }
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Frame::Browse { selected, .. } = &self.frame else {
            return;
        };
        let agents: Vec<String> = shown(cx)
            .iter()
            .map(|agent| agent.pubkey().to_owned())
            .collect();
        let index = selected
            .as_ref()
            .and_then(|pubkey| agents.iter().position(|p| p == pubkey));
        let key = event.keystroke.key.as_str();
        match key {
            "enter" => {
                if let Some(pubkey) = selected.clone() {
                    cx.stop_propagation();
                    self.build(pubkey, window, cx);
                }
            }
            "escape" => {
                if selected.is_some() {
                    cx.stop_propagation();
                    self.browse(None, cx);
                }
            }
            _ => {
                if let Some(next) = step(key, index, agents.len(), card::columns(window)) {
                    cx.stop_propagation();
                    self.browse(agents.get(next).cloned(), cx);
                }
            }
        }
    }

    fn header(
        &self,
        building: Option<&Agent>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> PanelHeader {
        let service = Agents2::global(cx);
        let ready = service
            .as_ref()
            .is_some_and(|service| *service.read(cx).status() == Status::Ready);
        let header = PanelHeader::new().joined(true);
        match building {
            Some(agent) => {
                let pubkey = agent.pubkey.clone();
                header
                    .leading(
                        IconButton::new("all-agents", IconName::ArrowLeft)
                            .size(IconButtonSize::Sm)
                            .tooltip("All agents")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browse(Some(pubkey.clone()), cx)
                            })),
                    )
                    .leading(
                        div()
                            .px(rems(0.5))
                            .text_label()
                            .truncate()
                            .child(agent.name.clone()),
                    )
            }
            None => {
                let teams_open = matches!(self.frame, Frame::Browse { teams: true, .. });
                let header = header.label(Some(IconName::Robot), "Agents").when(
                    self.tab == TabId::Yours && ready && !types(cx).is_empty(),
                    |this| this.action(self.more(teams_open, window, cx)),
                );
                #[cfg(feature = "voltage-sandbox")]
                let header = header.when(
                    self.tab == TabId::Yours
                        && ready
                        && type_of(crate::agents2::remote_migration::TARGET, cx).is_some(),
                    |header| {
                        header.action(
                            super::migration_controls::Button::new(
                                "agents-move-remote",
                                "Move to remote",
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.open_migration(None, cx))),
                        )
                    },
                );
                header
            }
        }
    }

    /// "More agent actions" and its menu.
    fn more(&self, teams_open: bool, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let trigger = IconButton::new("more-agent-actions", IconName::DotsThree)
            .size(IconButtonSize::Sm)
            .tooltip("More agent actions")
            .on_click(cx.listener(|this, _, _, cx| {
                this.more_open = !this.more_open;
                cx.notify();
            }));
        let menu = self.more_open.then(|| {
            // Hovering the trigger checks every provider.
            let check = |cx: &mut App| {
                for kind in types(cx) {
                    if let Some(provider) = kind.kind.provider(cx) {
                        provider.check(cx);
                    }
                }
            };
            let providers = div()
                .id("agents-providers-trigger")
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered && !this.providers_open {
                        this.providers_open = true;
                        check(cx);
                        cx.notify();
                    }
                }))
                .child(
                    MenuItem::new("agents-providers", "Provider settings")
                        .submenu()
                        .highlighted(self.providers_open)
                        // Opens it, as hovering does; a press never closes
                        // what the pointer just opened.
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.providers_open {
                                this.providers_open = true;
                                check(cx);
                                cx.notify();
                            }
                        })),
                );
            let teams = div()
                .id("agents-teams-row")
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if *hovered && this.providers_open {
                        this.providers_open = false;
                        cx.notify();
                    }
                }))
                .child(
                    MenuItem::new("agents-teams", "Teams")
                        .disabled(teams_open)
                        .on_click(cx.listener(|this, _, _, cx| this.open_teams(cx))),
                );
            let submenu = self.providers_open.then(|| {
                let kinds = types(cx);
                Menu::new().children(
                    kinds
                        .iter()
                        .enumerate()
                        .map(|(index, kind)| self.provider_row(index, kind, cx)),
                )
            });
            // The menu hangs from the page's right edge, so the submenu
            // opens to its left.
            div()
                .flex()
                .items_start()
                .gap(px(4.))
                .children(submenu)
                .child(Menu::new().child(providers).child(teams))
        });
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.more_open = false;
                this.providers_open = false;
                cx.notify();
            });
        });
        div()
            .relative()
            .child(trigger)
            .children(menu.map(|menu| overlay::dropdown(window, Align::End, menu, dismiss)))
            .into_any_element()
    }

    /// One agent's card in the listbox.
    fn card(agent: &Shown, active: bool, page: &WeakEntity<Self>, cx: &App) -> AnyElement {
        let pubkey = agent.pubkey().to_owned();
        let page = page.clone();
        let (name, picture, relay, kind) = match agent {
            Shown::Here(agent) => (
                agent.name.clone(),
                agent_picture_url(agent, cx),
                agent.relay.clone(),
                type_of(&agent.agent_type, cx),
            ),
            Shown::Elsewhere(agent) => (
                agent.name.clone(),
                agent.picture.clone(),
                agent.relay.clone(),
                type_of(&agent.agent_type, cx),
            ),
        };
        let detail = match agent {
            Shown::Here(agent) => summary_of(agent, kind.as_ref()),
            Shown::Elsewhere(agent) if crate::agents2::is_hosted(&agent.agent_type, cx) => {
                "Hosted".into()
            }
            Shown::Elsewhere(_) => "On another device".into(),
        };
        AgentCard::new(
            SharedString::from(format!("agent2-{pubkey}")),
            name,
            picture,
            relay,
        )
        .selected(active)
        .unavailable(kind.is_none())
        .icon(type_icon(kind.as_ref(), 16., cx))
        .detail(detail)
        .on_click(move |event: &ClickEvent, window, cx| {
            let _ = page.update(cx, |this, cx| {
                window.focus(&this.grid, cx);
                if event.click_count() >= 2 {
                    this.build(pubkey.clone(), window, cx);
                } else {
                    this.browse((!active).then(|| pubkey.clone()), cx);
                }
            });
        })
        .into_any_element()
    }

    /// Your agents: the states before the grid, or the grid.
    fn yours(&self, selected: Option<&str>, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let Some(service) = Agents2::global(cx) else {
            return div().into_any_element();
        };
        let status = service.read(cx).status().clone();
        let agents = shown(cx);
        let has_types = !types(cx).is_empty();
        match status {
            Status::Unavailable => {
                return empty_state(
                    "\u{2726}",
                    "Agents runs in the desktop app",
                    "Agent keys stay in the desktop app's native custody.",
                    None,
                    &c,
                )
                .into_any_element();
            }
            Status::Error(error) => {
                return empty_state(
                    "!",
                    "Agents could not load",
                    if error.is_empty() {
                        "Try again.".to_string()
                    } else {
                        error
                    },
                    None,
                    &c,
                )
                .into_any_element();
            }
            Status::Ready if !has_types && agents.is_empty() => {
                return empty_state(
                    "\u{2726}",
                    "No agents yet",
                    "Enable an agent type plugin to make one.",
                    None,
                    &c,
                )
                .into_any_element();
            }
            _ => {}
        }
        let page = cx.entity().downgrade();
        let cards: Vec<AnyElement> = agents
            .iter()
            .map(|agent| Self::card(agent, Some(agent.pubkey()) == selected, &page, cx))
            .collect();
        let tile = has_types.then(|| {
            card::new_agent_tile(
                status != Status::Ready,
                cx.listener(|this, _, window, cx| this.new_agent(window, cx)),
                cx,
            )
        });
        card::grid(window)
            .id("agents2-grid")
            .track_focus(&self.grid)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.key_down(event, window, cx)
            }))
            .children(tile)
            .children(cards)
            .into_any_element()
    }

    fn peek(&self, agent: &Shown, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let identity = |name: String,
                        picture: Option<String>,
                        relay: String,
                        line: String,
                        cx: &mut Context<Self>| {
            div()
                .flex()
                .items_start()
                .gap(rems(0.75))
                .child(div().flex_none().size(rems(4.)).child(card::portrait(
                    &name,
                    picture.as_deref(),
                    &relay,
                    4.,
                    cx,
                )))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(rems(0.125))
                        .child(div().text_heading().truncate().child(name))
                        .child(div().text_body_sm().text_color(c.text_subtle).child(line)),
                )
                .child(
                    IconButton::new("peek-close", IconName::X)
                        .size(IconButtonSize::Sm)
                        .tooltip("Close")
                        .on_click(cx.listener(|this, _, _, cx| this.browse(None, cx))),
                )
        };
        let agent = match agent {
            Shown::Elsewhere(agent) if crate::agents2::is_hosted(&agent.agent_type, cx) => {
                let pubkey = agent.pubkey.clone();
                return side_body()
                    .child(identity(
                        agent.name.clone(),
                        agent.picture.clone(),
                        agent.relay.clone(),
                        "Hosted".into(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child("Its type's provider holds its key and runs it."),
                    )
                    .child(
                        div().flex().child(
                            Button::new("hosted-delete", "Delete agent")
                                .icon(IconName::Trash)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_hosted(pubkey.clone(), cx)
                                })),
                        ),
                    );
            }
            Shown::Elsewhere(agent) => {
                return side_body()
                    .child(identity(
                        agent.name.clone(),
                        agent.picture.clone(),
                        agent.relay.clone(),
                        "On another device".into(),
                        cx,
                    ))
                    .child(div().text_body_sm().text_color(c.text_subtle).child(
                        "Its key is on another of your devices, which runs it. Open Agents there to change or delete it.",
                    ));
            }
            Shown::Here(agent) => agent,
        };
        let kind = type_of(&agent.agent_type, cx);
        let summary = summary_of(agent, kind.as_ref());
        let peek = kind
            .as_ref()
            .and_then(|kind| kind.kind.peek(agent, window, cx));
        let line = match &kind {
            None => "Type unavailable".to_string(),
            Some(kind) => {
                let title = kind.kind.title().to_string();
                if peek.is_none() && summary != title {
                    format!("{title} · {summary}")
                } else {
                    title
                }
            }
        };
        let channels = Agents2::global(cx)
            .map(|service| service.read(cx).channel_choices(cx))
            .unwrap_or_default();
        let pubkey = agent.pubkey.clone();
        side_body()
            .child(identity(
                agent.name.clone(),
                agent_picture_url(agent, cx),
                agent.relay.clone(),
                line,
                cx,
            ))
            .child(
                Button::new("peek-open", "Open")
                    .prominent()
                    .full_width()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.build(pubkey.clone(), window, cx)
                    })),
            )
            .map(|this| match (peek, &kind) {
                (Some(peek), _) => this.child(
                    div()
                        .py(rems(0.75))
                        .px(rems(1.))
                        .rounded(radius::CONTAINER)
                        .bg(c.surface_inset)
                        .child(peek),
                ),
                (None, None) => this.child(div().text_body_sm().text_color(c.text_subtle).child(
                    "Enable the plugin that provides this agent's type to run or change it.",
                )),
                (None, Some(_)) => this,
            })
            .child(section(
                "Wakes on",
                attention_summary(agent, &channels, &c),
                &c,
            ))
    }
}

/// The community the agent was created for, if it is added.
fn agent_community(agent: &Agent, cx: &App) -> Option<Entity<crate::community::Community>> {
    let relay = crate::ui::agents::inventory::origin(&agent.relay);
    cx.global::<crate::community::AppState>()
        .communities
        .iter()
        .find(|community| community.read(cx).origin.http() == relay)
        .cloned()
}

/// The agent's picture URL: the one its host saved, so a change shows
/// before its community has the new profile, else (a host from before
/// pictures) the one its community's profiles have.
fn agent_picture_url(agent: &Agent, cx: &App) -> Option<String> {
    let picture = match &agent.picture {
        Some(picture) => picture.clone(),
        None => agent_community(agent, cx)?
            .read(cx)
            .snapshot
            .profiles
            .get(&agent.pubkey)?
            .picture
            .clone()?,
    };
    (!picture.is_empty()).then_some(picture)
}

/// The agent's profile picture, through its community's media access.
pub(crate) fn agent_picture(agent: &Agent, cx: &App) -> Option<ImageSource> {
    let picture = agent_picture_url(agent, cx)?;
    card::picture_source(&picture, &agent.relay, cx)
}

pub(super) fn side_body() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(1.25))
        .pt(rems(1.))
        .px(rems(1.25))
        .pb(rems(1.5))
}

fn section(title: &'static str, content: impl IntoElement, c: &Colors) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .child(div().text_caption().text_color(c.text_subtle).child(title))
        .child(content)
}

impl Render for Agents2Page {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let Some(service) = Agents2::global(cx) else {
            return div();
        };
        // A deleted or out-of-scope agent's Build falls back to the grid.
        let building = match &self.frame {
            Frame::Build { pubkey } => service.read(cx).find(pubkey).cloned(),
            _ => None,
        };
        if building.is_none() && self.build.is_some() {
            self.build = None;
            self.frame = Frame::Browse {
                selected: None,
                teams: false,
            };
        }
        let (selected, teams_open) = match &self.frame {
            Frame::Browse { selected, teams } => (
                selected
                    .as_ref()
                    .and_then(|pubkey| shown(cx).into_iter().find(|a| a.pubkey() == pubkey)),
                *teams,
            ),
            Frame::Build { .. } => (None, false),
        };
        let narrow =
            f32::from(window.viewport_size().width) < NARROW_REM * f32::from(window.rem_size());

        let body: AnyElement = match (&building, &self.build) {
            (Some(_), Some(view)) => view.clone().into_any_element(),
            _ => {
                let yours = self.tab == TabId::Yours;
                let side_key = match (yours, teams_open) {
                    (false, _) => None,
                    (true, true) => Some("teams".to_string()),
                    (true, false) => selected.as_ref().map(|agent| agent.pubkey().to_owned()),
                };
                if side_key != self.side_key {
                    if side_key.is_some() {
                        self.side_opened += 1;
                    }
                    self.side_key = side_key.clone();
                }
                let side: Option<AnyElement> = match (yours, teams_open) {
                    (false, _) => None,
                    (true, true) => self.teams.clone().map(|panel| panel.into_any_element()),
                    (true, false) => selected
                        .as_ref()
                        .map(|agent| self.peek(agent, window, cx).into_any_element()),
                };
                let tab = self.tab;
                let tabs = TABS
                    .iter()
                    .fold(Tabs::new(TabVariant::Panel), |tabs, (id, label)| {
                        let id = *id;
                        tabs.tab(
                            Tab::new(SharedString::from(format!("agents-tab-{label}")), *label)
                                .selected(tab == id)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_tab(id, window, cx)
                                })),
                        )
                    });
                let content: AnyElement = match tab {
                    TabId::Yours => {
                        self.yours(selected.as_ref().map(|agent| agent.pubkey()), window, cx)
                    }
                    TabId::Import => self
                        .import
                        .clone()
                        .map(|view| view.into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                    TabId::Browse => self
                        .browse
                        .clone()
                        .map(|view| view.into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                };
                let viewport = f32::from(window.viewport_size().height);
                let rem = f32::from(window.rem_size());
                // `clamp(space-4, 6vh, space-16 + space-4)`.
                let top = (viewport * 0.06).clamp(rem, 5. * rem);
                let inventory = div()
                    .id("agents2-inventory")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(1.5))
                            .w_full()
                            .max_w(rems(48.))
                            .mx_auto()
                            .pt(px(top))
                            .px(rems(1.5))
                            .pb(rems(2.))
                            .child(tabs)
                            .child(content),
                    );
                let reduce = cx.reduce_motion();
                let side = side.map(|side| {
                    let panel = div()
                        .id("agents2-side")
                        .flex_none()
                        .w(rems(if narrow { 18. } else { 22. }))
                        .h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .border_l_1()
                        .border_color(c.border_standard)
                        .bg(c.surface_panel)
                        .relative()
                        .child(side);
                    let enter = Animation::new(duration::FAST)
                        .with_easing(|t| easing::cubic_bezier(easing::ENTER, t));
                    panel.with_animation(
                        ElementId::Name(format!("agents2-side-{}", self.side_opened).into()),
                        enter,
                        move |panel, delta| {
                            if reduce {
                                panel
                            } else {
                                panel.opacity(delta).left(rems(1. - delta))
                            }
                        },
                    )
                });
                div()
                    .flex()
                    .size_full()
                    .min_h_0()
                    .child(inventory)
                    .children(side)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .text_color(c.text_standard)
            .text_body()
            .child(self.header(building.as_ref(), window, cx))
            .child(div().flex_1().min_h_0().child(body))
            .children(self.create.clone())
            .children(self.provider_dialog(window, cx))
            .map(|page| {
                #[cfg(feature = "voltage-sandbox")]
                let page = page.children(self.migration.clone());
                page
            })
    }
}

// ----- Build -----

pub struct Build {
    pubkey: String,
    name: Entity<InputState>,
    /// The name the field last showed, to follow outside renames.
    shown: String,
    /// Escape blurs too, and blur saves; this skips that one save.
    cancelled: bool,
    name_error: Option<SharedString>,
    /// A picture is saving.
    picture_busy: bool,
    picture_error: Option<SharedString>,
    /// The avatar editor, while open.
    avatar: Option<(Entity<AvatarEditor>, [Subscription; 2])>,
    copied: usize,
    copied_shown: bool,
    tabs: Entity<AgentTabs>,
    delete: Entity<AgentDelete>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Removed> for Build {}

impl Build {
    fn new(pubkey: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = Agents2::global(cx)
            .and_then(|service| {
                service
                    .read(cx)
                    .find(&pubkey)
                    .map(|agent| agent.name.clone())
            })
            .unwrap_or_default();
        let name = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(current.clone(), window, cx);
            state
        });
        let tabs = cx.new(|cx| AgentTabs::new(pubkey.clone(), window, cx));
        let delete = cx.new(|cx| AgentDelete::new(pubkey.clone(), cx));
        let mut subscriptions = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => window.blur(cx),
                    InputEvent::Blur => this.commit(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe(&delete, |_, _, _: &Removed, cx| cx.emit(Removed)),
        ];
        if let Some(service) = Agents2::global(cx) {
            subscriptions.push(cx.observe_in(&service, window, |this, _, window, cx| {
                this.follow(window, cx)
            }));
        }
        Self {
            pubkey,
            name,
            shown: current,
            cancelled: false,
            name_error: None,
            picture_busy: false,
            picture_error: None,
            avatar: None,
            copied: 0,
            copied_shown: false,
            tabs,
            delete,
            _subscriptions: subscriptions,
        }
    }

    fn current(&self, cx: &App) -> Option<Agent> {
        Agents2::global(cx)?.read(cx).find(&self.pubkey).cloned()
    }

    /// Shows a rename made elsewhere unless the field is being edited.
    fn follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.current(cx) else {
            return;
        };
        if agent.name != self.shown && !self.name.read(cx).focus_handle(cx).is_focused(window) {
            self.shown = agent.name.clone();
            self.name
                .update(cx, |state, cx| state.set_value(agent.name, window, cx));
        }
        cx.notify();
    }

    /// Saves on Enter or blur; an empty field puts the name back.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.cancelled) {
            return;
        }
        let Some(agent) = self.current(cx) else {
            return;
        };
        let next = self.name.read(cx).value().trim().to_string();
        if next.is_empty() {
            self.name.update(cx, |state, cx| {
                state.set_value(agent.name.clone(), window, cx)
            });
            return;
        }
        if next == agent.name {
            return;
        }
        self.name_error = None;
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        let task = service.update(cx, |service, cx| {
            service.save(
                &agent.pubkey,
                AgentChange {
                    name: Some(next),
                    ..Default::default()
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.name_error = Some(error.into());
                    this.name
                        .update(cx, |state, cx| state.set_value(agent.name, window, cx));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Opens the avatar editor on the agent's picture; Done saves it, which
    /// its host then publishes in its profile (`AvatarEditor`).
    fn open_avatar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.current(cx) else {
            return;
        };
        if self.picture_busy || self.avatar.is_some() {
            return;
        }
        if agent.picture.is_none() {
            self.picture_error = Some(crate::agents2::PICTURES_UNSUPPORTED.into());
            return cx.notify();
        }
        let community = agent_community(&agent, cx);
        let picture = agent.picture.clone().unwrap_or_default();
        let editor = cx.new(|cx| AvatarEditor::new(picture, community, window, cx));
        let subscriptions = [
            cx.subscribe(&editor, |this, _, AvatarDone(picture), cx| {
                this.avatar = None;
                this.change_picture(picture.clone(), cx);
            }),
            cx.subscribe(&editor, |this, _, _: &AvatarDismissed, cx| {
                this.avatar = None;
                cx.notify();
            }),
        ];
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.avatar = Some((editor, subscriptions));
        self.picture_error = None;
        cx.notify();
    }

    /// Saves `picture`, an uploaded image's URL; empty removes it.
    fn change_picture(&mut self, picture: String, cx: &mut Context<Self>) {
        self.picture_busy = true;
        self.picture_error = None;
        let task = self.save_picture(picture, cx);
        cx.spawn(async move |this, cx| {
            let saved = task.await;
            let _ = this.update(cx, |this, cx| {
                this.picture_busy = false;
                this.picture_error = saved.err().map(Into::into);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_picture(
        &mut self,
        picture: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let Some(service) = Agents2::global(cx) else {
            return Task::ready(Err("Agents is not running".into()));
        };
        let pubkey = self.pubkey.clone();
        service.update(cx, |service, cx| {
            service.save(
                &pubkey,
                AgentChange {
                    picture: Some(picture),
                    ..Default::default()
                },
                cx,
            )
        })
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.current(cx) else {
            return;
        };
        self.cancelled = true;
        self.name
            .update(cx, |state, cx| state.set_value(agent.name, window, cx));
        window.blur(cx);
        // Escape may not blur a field that is not focused; nothing to skip.
        if !self.name.read(cx).focus_handle(cx).is_focused(window) {
            cx.defer_in(window, |this, _, _| this.cancelled = false);
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.pubkey.clone()));
        self.copied += 1;
        self.copied_shown = true;
        let generation = self.copied;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == generation {
                    this.copied_shown = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for Build {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let Some(agent) = self.current(cx) else {
            return div();
        };
        let kind = type_of(&agent.agent_type, cx);
        let narrow =
            f32::from(window.viewport_size().width) < NARROW_REM * f32::from(window.rem_size());
        let facts = div()
            .flex()
            .flex_col()
            .gap(rems(0.25))
            .text_body_sm()
            .child(div().text_color(c.text_subtle).child("Key"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .min_w_0()
                    .child(
                        div()
                            .font_family("JetBrains Mono")
                            .truncate()
                            .child(short_key(&agent.pubkey, 8)),
                    )
                    .child(
                        IconButton::new("copy-key", IconName::Copy)
                            .size(IconButtonSize::Sm)
                            .icon_size(14.)
                            .tooltip(if self.copied_shown {
                                "Copied"
                            } else {
                                "Copy public key"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    ),
            );
        let name_field = div()
            .flex()
            .flex_col()
            .gap(rems(0.25))
            .when(narrow, |this| this.flex_1().min_w(rems(12.)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.cancel(window, cx);
                }
            }))
            .child(TextField::new(&self.name).invalid(self.name_error.is_some()))
            .children(problem(self.name_error.as_ref(), &c));
        // The compact avatar editor: the picture, or the draft while the
        // editor is open, with the edit badge over its lower right.
        let draft = self
            .avatar
            .as_ref()
            .map(|(editor, _)| editor.read(cx).preview(cx));
        let artwork = match draft {
            Some(Preview::Emoji { art, color }) => div()
                .size_full()
                .rounded(radius::CONTAINER)
                .overflow_hidden()
                .bg(color)
                .children(art.map(|art| img(art).size_full()))
                .into_any_element(),
            draft => {
                let shown = match draft {
                    Some(Preview::Picture(picture)) => agent_community(&agent, cx)
                        .filter(|_| !picture.is_empty())
                        .map(|community| {
                            crate::ui::media::avatar(&picture, community.read(cx).session.media())
                        }),
                    _ => agent_picture(&agent, cx),
                };
                Avatar::new(agent.name.clone())
                    .size(AvatarSize::Px(96.))
                    .squircle()
                    .when_some(shown, |avatar, picture| avatar.image(picture))
                    .into_any_element()
            }
        };
        let page = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| {
                this.avatar = None;
                cx.notify();
            });
        });
        let popover = self
            .avatar
            .as_ref()
            .map(|(editor, _)| avatar_popover(window, editor.clone(), dismiss));
        let has_picture = agent.picture.as_ref().is_some_and(|p| !p.is_empty());
        let badge = div()
            .absolute()
            .left(px(96. * 0.859375 - 16.))
            .top(px(96. * 0.859375 - 16.))
            .child(
                IconButton::new(
                    "agent-picture",
                    if has_picture {
                        IconName::PencilSimple
                    } else {
                        IconName::Plus
                    },
                )
                .size(IconButtonSize::Sm)
                .variant(buzz_ui::ButtonVariant::Prominent)
                .shape(buzz_ui::IconButtonShape::Round)
                .disabled(self.picture_busy)
                .tooltip("Edit avatar")
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.avatar.take().is_some() {
                        cx.notify();
                    } else {
                        this.open_avatar(window, cx);
                    }
                })),
            )
            .children(popover);
        let picture = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(rems(0.5))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .size(rems(6.))
                    .child(artwork)
                    .child(badge),
            )
            .children(problem(self.picture_error.as_ref(), &c));
        let rail = div()
            .id("agents2-rail")
            .flex()
            .gap(rems(1.))
            .py(rems(1.5))
            .px(rems(1.25))
            .border_color(c.border_standard)
            .overflow_y_scroll()
            .map(|this| {
                if narrow {
                    this.flex_row().flex_wrap().items_center().border_b_1()
                } else {
                    this.flex_col()
                        .w(rems(15.))
                        .flex_none()
                        .h_full()
                        .border_r_1()
                }
            })
            .child(picture)
            .child(name_field)
            .child(facts)
            .child(
                div()
                    .flex()
                    .when(!narrow, |this| this.mt_auto())
                    .child(self.delete.clone()),
            );
        let work = div()
            .id("agents2-work")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(rems(44.))
                    .pt(rems(1.))
                    .px(rems(1.5))
                    .pb(rems(2.))
                    .when(kind.is_none(), |this| {
                        this.child(div().mb(rems(1.)).text_body_sm().text_color(c.text_subtle).child(
                            "Enable the plugin that provides this agent's type to change its \
                             settings. Its attention can still be edited.",
                        ))
                    })
                    .child(self.tabs.clone()),
            );
        div()
            .flex()
            .size_full()
            .min_h_0()
            .map(|this| {
                if narrow {
                    this.flex_col()
                } else {
                    this.flex_row()
                }
            })
            .child(rail)
            .child(work)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn arrows_move_by_column_and_stop_at_the_edges() {
        // Seven tiles in rows of three.
        assert_eq!(step("right", None, 7, 3), Some(0));
        assert_eq!(step("right", Some(0), 7, 3), Some(1));
        assert_eq!(step("left", Some(0), 7, 3), Some(0));
        assert_eq!(step("right", Some(6), 7, 3), Some(6));
        assert_eq!(step("down", Some(1), 7, 3), Some(4));
        assert_eq!(step("down", Some(4), 7, 3), Some(4), "no tile below");
        assert_eq!(step("down", Some(3), 7, 3), Some(6));
        assert_eq!(step("up", Some(1), 7, 3), Some(1));
        assert_eq!(step("up", Some(5), 7, 3), Some(2));
        assert_eq!(step("home", Some(5), 7, 3), Some(0));
        assert_eq!(step("end", Some(0), 7, 3), Some(6));
        assert_eq!(step("x", Some(0), 7, 3), None);
        assert_eq!(step("down", None, 0, 3), None);
    }

    #[test]
    fn keys_show_as_short_npubs() {
        let key = buzz_identity::Identity::ephemeral().pubkey_hex();
        let short = short_key(&key, 8);
        assert!(short.starts_with("npub…"));
        assert_eq!(short.chars().count(), 13);
        assert_eq!(short_key("nothex", 8), "");
    }
}
