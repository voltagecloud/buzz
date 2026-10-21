//! The Projects page (`ProjectsPage.tsx`, `projects.css`): the recent
//! directory, and an entity with its tabs. Every route is a visit, so a
//! click goes through navigation and Back restores the previous route.

use super::{PAGE, format_public_key, locale_time};
use crate::community::{AppState, Community};
use crate::ui::markdown;
use crate::ui::navigation::{self, Place, Target};
use buzz_relay_client::projects::{
    Entity as Announcement, EntityDetail, EntityFailure, EntityKind, EntityRoute, EntityTab,
    GitRead, GitSnapshot, ItemKind, RouteKind, item_title, value, values,
};
use buzz_relay_client::{ChannelSummary, Session, SessionStatus};
use buzz_ui::{
    ActiveTheme, Button, Colors, Icon, IconButton, IconName, Tab, TabVariant, Tabs, TypeStyled,
    tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// The tabs, Overview first (`tabs`).
const TABS: [(Option<EntityTab>, &str); 7] = [
    (None, "Overview"),
    (Some(EntityTab::Files), "Files"),
    (Some(EntityTab::Commits), "Commits"),
    (Some(EntityTab::Prs), "Pull requests"),
    (Some(EntityTab::Issues), "Issues"),
    (Some(EntityTab::Contributors), "Contributors"),
    (Some(EntityTab::Channels), "Channels"),
];

fn label(tab: Option<EntityTab>) -> &'static str {
    TABS.iter()
        .find(|(t, _)| *t == tab)
        .map_or("Overview", |(_, label)| label)
}

/// What the page was asked to show.
#[derive(Clone, Debug, PartialEq)]
enum Destination {
    Directory,
    Entity(EntityRoute),
    /// Route params the page cannot open.
    Invalid,
}

struct Detail {
    detail: EntityDetail,
    git: Option<GitSnapshot>,
    /// The channel the repository's Git access is bound to.
    git_channel: Option<String>,
    /// That channel was in the viewer's list, so leaving it revokes the
    /// content shown.
    git_channel_listed: bool,
    git_error: Option<EntityFailure>,
    /// The Channels tab's accessible channels, in link order.
    channels: Vec<ChannelSummary>,
}

enum Loaded {
    Directory(Vec<Announcement>),
    Detail(Box<Detail>),
}

enum State {
    /// Waiting for a connected community.
    Waiting,
    Loading,
    Failed(EntityFailure),
    Ready(Loaded),
}

pub struct ProjectsPage {
    community: Option<gpui_kit::Entity<Community>>,
    destination: Destination,
    /// The file the Files tab shows, for this visit only.
    path: Option<String>,
    state: State,
    /// Bumped by every visit, retry and file choice.
    generation: u64,
    /// The community and generation the state belongs to.
    loaded: Option<(EntityId, u64)>,
    task: Option<Task<()>>,
    copied: Option<SharedString>,
    scroll: ScrollHandle,
    _app: Subscription,
    _community: Option<Subscription>,
}

impl ProjectsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _ = window;
        let mut page = Self {
            community: None,
            destination: Destination::Directory,
            path: None,
            state: State::Waiting,
            generation: 0,
            loaded: None,
            task: None,
            copied: None,
            scroll: ScrollHandle::new(),
            _app: cx.observe_global::<AppState>(|this, cx| this.sync(cx)),
            _community: None,
        };
        page.sync(cx);
        page
    }

    /// A visit: `params` is an entity route, or `None` for the directory.
    pub fn open(
        &mut self,
        params: Option<&serde_json::Value>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.destination = match params {
            None => Destination::Directory,
            Some(params) => EntityRoute::from_params(params)
                .map(Destination::Entity)
                .unwrap_or(Destination::Invalid),
        };
        self.path = None;
        self.copied = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.state = State::Waiting;
        self.sync(cx);
        cx.notify();
    }

    fn select_file(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        self.path = path;
        self.reload(cx);
    }

    /// Follows the selected community and starts the load the current
    /// visit needs once it is connected.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let current = AppState::current(cx);
        if current.as_ref().map(|c| c.entity_id()) != self.community.as_ref().map(|c| c.entity_id())
        {
            self._community = current.as_ref().map(|community| {
                // Re-render too: leaving a repository's channel revokes
                // the content shown.
                cx.observe(community, |this, _, cx| {
                    this.sync(cx);
                    cx.notify();
                })
            });
            self.community = current;
            self.generation += 1;
            self.state = State::Waiting;
            self.task = None;
        }
        let Some(community) = self.community.clone() else {
            return;
        };
        let key = (community.entity_id(), self.generation);
        let community = community.read(cx);
        if self.loaded == Some(key) || community.snapshot.status != SessionStatus::Ready {
            return;
        }
        self.loaded = Some(key);
        self.state = State::Loading;
        let session = community.session.clone();
        let listed = community.snapshot.channels.clone();
        let destination = self.destination.clone();
        let path = self.path.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = load(session, listed, destination, path).await;
            let _ = this.update(cx, |this, cx| {
                if this.loaded == Some(key) {
                    // The visit waiting on this page opens or fails with it
                    // (`PageNavigation.complete`, `entityFailure`).
                    let failed = result.as_ref().err().copied();
                    navigation::with_workspace(cx, move |workspace, _, cx| {
                        use crate::ui::channel_states::Reason;
                        let _ = match failed {
                            None => workspace.settle_visit(PAGE.key(), cx),
                            Some(EntityFailure::NotFound) => {
                                workspace.fail_visit(PAGE.key(), Reason::NotFound, cx)
                            }
                            Some(EntityFailure::Denied) => {
                                workspace.fail_visit(PAGE.key(), Reason::Denied, cx)
                            }
                            Some(EntityFailure::InvalidTarget) => {
                                workspace.fail_visit(PAGE.key(), Reason::Invalid, cx)
                            }
                            Some(EntityFailure::Unavailable) => {
                                workspace.fail_visit(PAGE.key(), Reason::Page, cx)
                            }
                        };
                    });
                    this.state = match result {
                        Ok(loaded) => State::Ready(loaded),
                        Err(failure) => State::Failed(failure),
                    };
                    this.task = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// Opens `route` as a new visit.
    fn go(&self, route: EntityRoute, cx: &mut App) {
        let community = self
            .community
            .as_ref()
            .map(|community| community.read(cx).origin.to_string());
        let target = Target {
            community,
            place: Place::Route {
                page: PAGE,
                params: route.params(),
                version: crate::ui::pages::routes(PAGE, cx).map(|routes| routes.version),
            },
        };
        navigation::with_workspace(cx, move |workspace, window, cx| {
            workspace.navigate(target, window, cx)
        });
    }

    fn open_channel(&self, channel: String, cx: &mut App) {
        let community = self
            .community
            .as_ref()
            .map(|community| community.read(cx).origin.to_string());
        let target = Target::channel(community, channel);
        navigation::with_workspace(cx, move |workspace, window, cx| {
            workspace.navigate(target, window, cx)
        });
    }
}

/// `channels.get` after `channels.resolve`: the listed or readable
/// channels among `ids`, in order.
async fn accessible(
    session: &Session,
    listed: &[ChannelSummary],
    ids: &[String],
) -> Result<Vec<ChannelSummary>, EntityFailure> {
    let unknown: Vec<String> = ids
        .iter()
        .filter(|id| !listed.iter().any(|c| c.id == **id))
        .cloned()
        .collect();
    let resolved = if unknown.is_empty() {
        Vec::new()
    } else {
        session
            .resolve_channels(unknown)
            .await
            .map_err(|_| EntityFailure::Unavailable)?
    };
    Ok(ids
        .iter()
        .filter_map(|id| {
            listed
                .iter()
                .chain(&resolved)
                .find(|c| c.id == *id)
                .cloned()
        })
        .collect())
}

/// `Destination`'s `load`: the directory, or the entity with what its tab
/// shows. Overview tolerates missing repository content; a content tab
/// fails without it.
async fn load(
    session: Arc<Session>,
    listed: Arc<[ChannelSummary]>,
    destination: Destination,
    path: Option<String>,
) -> Result<Loaded, EntityFailure> {
    let projects = session.projects();
    let route = match destination {
        Destination::Directory => return Ok(Loaded::Directory(projects.list().await?)),
        Destination::Invalid => return Err(EntityFailure::InvalidTarget),
        Destination::Entity(route) => route,
    };
    let detail = projects.load(route.clone()).await?;
    let tab = route.tab;
    let mut loaded = Detail {
        channels: Vec::new(),
        git: None,
        git_channel: None,
        git_channel_listed: false,
        git_error: None,
        detail,
    };
    if tab == Some(EntityTab::Channels) {
        loaded.channels = accessible(&session, &listed, &loaded.detail.channels).await?;
    }
    if route.id.is_some()
        || matches!(
            tab,
            Some(EntityTab::Prs | EntityTab::Issues | EntityTab::Channels)
        )
        || (route.kind == RouteKind::Project && tab.is_none())
    {
        return Ok(Loaded::Detail(Box::new(loaded)));
    }
    let repositories = &loaded.detail.repositories;
    let Some(repository) = repositories
        .iter()
        .find(|repo| repo.dtag == route.dtag)
        .or(repositories.first())
        .cloned()
    else {
        if !loaded.detail.unavailable_repositories.is_empty() {
            return Err(EntityFailure::Unavailable);
        }
        return Ok(Loaded::Detail(Box::new(loaded)));
    };
    let read = async {
        let channel = value(&repository.event, "buzz-channel").map(str::to_string);
        if let Some(channel) = &channel
            && accessible(&session, &listed, std::slice::from_ref(channel))
                .await?
                .is_empty()
        {
            return Err(EntityFailure::Denied);
        }
        let git = projects
            .read_git(GitRead {
                owner: repository.owner.clone(),
                dtag: repository.dtag.clone(),
                commit: route
                    .commit
                    .clone()
                    .filter(|_| route.kind == RouteKind::Repo),
                path,
            })
            .await?;
        Ok((git, channel))
    };
    match read.await {
        Ok((git, channel)) => {
            loaded.git_channel_listed = channel
                .as_ref()
                .is_some_and(|id| listed.iter().any(|c| c.id == *id));
            loaded.git_channel = channel;
            loaded.git = Some(git);
        }
        Err(failure) if tab.is_some() => return Err(failure),
        Err(failure) => loaded.git_error = Some(failure),
    }
    Ok(Loaded::Detail(Box::new(loaded)))
}

// ── rendering ──────────────────────────────────────────────────────────────

/// A `Button variant="link"` as `.project-list .buzz-button` styles it:
/// the control's padding and minimum height, wrapping text.
fn link(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    disabled: bool,
    c: &Colors,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .max_w_full()
        .min_h(rems(2.5))
        .px(rems(1.5))
        .py(rems(0.5))
        .rounded(radius::CAPSULE)
        .text_color(if disabled {
            c.text_unavailable
        } else {
            c.text_standard
        })
        .when(disabled, |d| d.cursor_not_allowed())
        .when(!disabled, |d| d.cursor_pointer().hover(|s| s.underline()))
        .child(div().min_w_0().child(text.into()))
}

/// `.project-list li`.
fn row(c: &Colors) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_baseline()
        .gap(rems(0.75))
        .py(rems(0.75))
        .border_b_1()
        .border_color(c.border_standard)
}

/// `.project-list`.
fn list() -> Div {
    div().flex().flex_col().my(rems(1.))
}

fn small(text: impl Into<SharedString>, c: &Colors) -> Div {
    div()
        .text_caption()
        .text_color(c.text_subtle)
        .child(text.into())
}

fn secondary(text: impl Into<SharedString>, c: &Colors) -> Div {
    div().text_color(c.text_subtle).child(text.into())
}

/// `.project-code`: preformatted text that scrolls, at most 65% of the
/// window high.
fn code(id: &str, text: &str, window: &Window, c: &Colors) -> AnyElement {
    let max = window.viewport_size().height * 0.65;
    div()
        .id(SharedString::from(format!("project-code-{id}")))
        .w_full()
        .max_h(max)
        .overflow_scroll()
        .p(rems(1.))
        .bg(c.surface_inset)
        .text_mono()
        .child(
            div().flex().flex_col().children(
                text.split('\n')
                    .map(|line| div().whitespace_nowrap().child(line.to_string())),
            ),
        )
        .into_any_element()
}

/// `Body`: Markdown with Buzz links routed through navigation.
/// Plain Markdown, as `ProjectsPage`'s is: no plugin renderers, and web
/// links go to the browser.
fn body(text: &str, community: &Community, cx: &App) -> AnyElement {
    crate::ui::links::with_mode(crate::ui::links::LinkMode::Browser, || {
        crate::ui::renderers::without_renderers(|| markdown::text(text, "projects", community, cx))
    })
}

impl Render for ProjectsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let content: AnyElement = match self.community.clone() {
            Some(community)
                if community.read(cx).snapshot.status == SessionStatus::Ready
                    && !matches!(self.state, State::Waiting) =>
            {
                self.destination_view(&community, window, cx)
            }
            community => {
                let connecting = community.is_some_and(|community| {
                    matches!(
                        community.read(cx).snapshot.status,
                        SessionStatus::Connecting | SessionStatus::Ready
                    )
                });
                div()
                    .child(div().text_title().child("Projects"))
                    .child(div().child(if connecting {
                        "Connecting to your community..."
                    } else {
                        "Select a community to browse projects."
                    }))
                    .into_any_element()
            }
        };
        div()
            .id("projects-page")
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p(rems(1.5))
            .text_body()
            .text_color(c.text_standard)
            .child(content)
    }
}

impl ProjectsPage {
    fn destination_view(
        &self,
        community: &gpui_kit::Entity<Community>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let route = match &self.destination {
            Destination::Entity(route) => Some(route),
            _ => None,
        };
        let content_tab = route.and_then(|r| r.tab).is_some_and(EntityTab::needs_git);
        let failure = match &self.state {
            State::Failed(failure) => Some(*failure),
            State::Ready(Loaded::Detail(detail)) if content_tab => {
                self.git_failure(detail, community, cx)
            }
            _ => None,
        };
        if let Some(failure) = failure {
            return div()
                .flex()
                .flex_col()
                .items_start()
                .child(div().text_title().child("This destination couldn’t open"))
                .child(div().child(if failure == EntityFailure::NotFound {
                    "The requested entity was not found in this community."
                } else {
                    "Repository content is unavailable."
                }))
                .child(
                    Button::new("projects-retry", "Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .into_any_element();
        }
        match &self.state {
            State::Ready(Loaded::Directory(entities)) => self.directory(entities, cx),
            State::Ready(Loaded::Detail(detail)) => match route {
                Some(route) => self.detail(detail, route, community, window, cx),
                None => div().into_any_element(),
            },
            _ => div()
                .child("Loading project...")
                .text_color(c.text_standard)
                .into_any_element(),
        }
    }

    /// The Git failure the visit shows: the read's, or denied once the
    /// viewer left the repository's channel.
    fn git_failure(
        &self,
        detail: &Detail,
        community: &gpui_kit::Entity<Community>,
        cx: &App,
    ) -> Option<EntityFailure> {
        if detail.git.is_some() && revoked(detail, community, cx) {
            return Some(EntityFailure::Denied);
        }
        detail.git_error
    }

    fn directory(&self, entities: &[Announcement], cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let rows =
            entities.iter().enumerate().map(|(index, entity)| {
                let route = entity.route();
                let disabled = route.is_none();
                row(&c)
                    .child(Icon::new(IconName::GitBranch).size(20.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                link(("project", index), entity.name.clone(), disabled, &c)
                                    .when(disabled, |d| {
                                        d.tooltip(buzz_ui::Tooltip::text(
                                    "This repository identifier cannot be opened with a Buzz link",
                                ))
                                    })
                                    .when_some(route, |d, route| {
                                        d.on_click(cx.listener(move |this, _, _, cx| {
                                            this.go(route.clone(), cx)
                                        }))
                                    }),
                            )
                            .child(
                                div()
                                    .my(rems(0.25))
                                    .when(!entity.description.is_empty(), |d| {
                                        d.child(entity.description.clone())
                                    }),
                            )
                            .child(small(format_public_key(&entity.owner), &c)),
                    )
            });
        div()
            .child(div().text_title().child("Projects"))
            .child(secondary("Recent projects and repositories", &c))
            .child(div().child(
                "Partial list, up to 100 announcements. Projects and repositories are shown separately, not as a grouped collection.",
            ))
            .child(list().children(rows))
            .when(entities.is_empty(), |d| {
                d.child(div().child("No recent projects or repositories found."))
            })
            .into_any_element()
    }

    fn detail(
        &self,
        loaded: &Detail,
        route: &EntityRoute,
        community: &gpui_kit::Entity<Community>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let detail = &loaded.detail;
        let entity = &detail.entity;
        let active = match route.kind {
            RouteKind::Pr => Some(EntityTab::Prs),
            RouteKind::Issue => Some(EntityTab::Issues),
            _ => route.tab,
        };
        let base = EntityRoute::entity(entity.kind, &entity.owner, &entity.dtag);
        let title = match &detail.item {
            Some(item) => item_title(item),
            None => entity.name.clone(),
        };
        let header = div()
            .flex()
            .items_start()
            .justify_between()
            .gap(rems(1.))
            .mb(rems(1.))
            .child(
                div()
                    .min_w_0()
                    .child(secondary(
                        format!(
                            "{} · {}",
                            match entity.kind {
                                EntityKind::Repo => "Repository",
                                EntityKind::Project => "Project",
                            },
                            format_public_key(&entity.owner)
                        ),
                        &c,
                    ))
                    .child(div().text_title().child(title)),
            )
            .child(self.copy_link(route, cx));
        let tabs = TABS.iter().enumerate().fold(
            Tabs::new(TabVariant::Panel),
            |tabs, (index, (tab, name))| {
                let tab = *tab;
                let base = base.clone();
                tabs.tab(
                    Tab::new(("project-tab", index), *name)
                        .selected(tab == active)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(base) = &base {
                                this.go(base.clone().with_tab(tab), cx)
                            }
                        })),
                )
            },
        );
        let section = div()
            .py(rems(1.25))
            .flex()
            .flex_col()
            .children(self.repository_switcher(loaded, active, &c, cx))
            .child(match &detail.item {
                Some(item) => self.item(loaded, item, base.as_ref(), community, &c, cx),
                None => match active {
                    None => self.overview(loaded, community, &c, cx),
                    Some(tab @ (EntityTab::Prs | EntityTab::Issues)) => {
                        self.items(detail, tab, &c, cx)
                    }
                    Some(EntityTab::Channels) => self.channels(loaded, &c, cx),
                    Some(tab) => match &loaded.git {
                        Some(git) if !revoked(loaded, community, cx) => {
                            self.git_content(git, tab, route, detail, window, &c, cx)
                        }
                        _ => div().child("No repository linked.").into_any_element(),
                    },
                },
            })
            .when(!detail.unavailable_repositories.is_empty(), |d| {
                d.child(div().child(format!(
                    "{} linked repositories are unavailable in this community.",
                    detail.unavailable_repositories.len()
                )))
            });
        div()
            .child(header)
            .when(detail.item.is_some(), |d| {
                d.child(div().flex().child(
                    link("project-entity", entity.name.clone(), false, &c).when_some(
                        base.clone(),
                        |d, base| {
                            d.on_click(cx.listener(move |this, _, _, cx| this.go(base.clone(), cx)))
                        },
                    ),
                ))
            })
            .child(tabs)
            .child(section)
            .into_any_element()
    }

    /// `CopyLink`: copies the route's canonical link.
    fn copy_link(&self, route: &EntityRoute, cx: &mut Context<Self>) -> AnyElement {
        let href = route.href();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(rems(0.5))
            .child(
                IconButton::new("project-copy-link", IconName::Copy)
                    .icon_size(18.)
                    .tooltip("Copy link")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(href.clone()));
                        this.copied = Some("Link copied".into());
                        cx.notify();
                    })),
            )
            .children(self.copied.clone())
            .into_any_element()
    }

    /// A project's Files, Commits and Contributors name the repository
    /// shown and link the others.
    fn repository_switcher(
        &self,
        loaded: &Detail,
        active: Option<EntityTab>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let detail = &loaded.detail;
        let tab = active.filter(|tab| tab.needs_git())?;
        if detail.entity.kind != EntityKind::Project {
            return None;
        }
        let primary = primary(detail)?;
        let others = detail
            .repositories
            .iter()
            .enumerate()
            .filter(|(_, repo)| repo.address != primary.address)
            .map(|(index, repo)| {
                let route = repo.route().map(|r| r.with_tab(Some(tab)));
                link(
                    ("project-repo", index),
                    repo.name.clone(),
                    route.is_none(),
                    c,
                )
                .when_some(route, |d, route| {
                    d.on_click(cx.listener(move |this, _, _, cx| this.go(route.clone(), cx)))
                })
            });
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .child(format!("Repository: {}", primary.name))
                .children(others)
                .into_any_element(),
        )
    }

    fn overview(
        &self,
        loaded: &Detail,
        community: &gpui_kit::Entity<Community>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = &loaded.detail;
        let entity = &detail.entity;
        let mut out =
            div()
                .flex()
                .flex_col()
                .child(body(&entity.description, community.read(cx), cx));
        if entity.kind == EntityKind::Project {
            let rows = detail.repositories.iter().enumerate().map(|(index, repo)| {
                let route = repo.route();
                let disabled = route.is_none();
                row(c)
                    .child(Icon::new(IconName::GitBranch).size(20.))
                    .child(
                        link(("project-member", index), repo.name.clone(), disabled, c)
                            .when(disabled, |d| {
                                d.tooltip(buzz_ui::Tooltip::text(
                                    "This repository identifier cannot be opened with a Buzz link",
                                ))
                            })
                            .when_some(route, |d, route| {
                                d.on_click(
                                    cx.listener(move |this, _, _, cx| this.go(route.clone(), cx)),
                                )
                            }),
                    )
            });
            out = out
                .child(div().text_heading().child("Repositories"))
                .child(list().children(rows))
                .when(detail.repositories.is_empty(), |d| {
                    d.child(div().child("No repositories linked."))
                });
        }
        let error = self.git_failure(loaded, community, cx);
        if let Some(error) = error {
            out = out.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .child(if error == EntityFailure::Denied {
                        "Repository files require access to the linked channel."
                    } else {
                        "Repository files are unavailable."
                    })
                    .child(
                        link("project-retry-files", "Retry files", false, c)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            );
        } else if let Some(readme) = loaded.git.as_ref().and_then(|git| git.readme.as_ref()) {
            out = out.child(body(readme, community.read(cx), cx));
        } else if entity.kind == EntityKind::Repo {
            out = out.child(div().child("No README in this repository."));
        }
        out.into_any_element()
    }

    fn items(
        &self,
        detail: &EntityDetail,
        tab: EntityTab,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = if tab == EntityTab::Prs {
            ItemKind::Pr
        } else {
            ItemKind::Issue
        };
        let rows = detail.items.iter().enumerate().filter_map(|(index, item)| {
            let repo = detail
                .repositories
                .iter()
                .find(|repo| values(item, "a").contains(&repo.address.as_str()))?;
            let route = EntityRoute::item(kind, &repo.owner, &repo.dtag, &item.id.to_hex());
            Some(
                row(c)
                    .child(
                        link(
                            ("project-item", index),
                            item_title(item),
                            route.is_none(),
                            c,
                        )
                        .when_some(route, |d, route| {
                            d.on_click(
                                cx.listener(move |this, _, _, cx| this.go(route.clone(), cx)),
                            )
                        }),
                    )
                    .child(small(repo.name.clone(), c)),
            )
        });
        div()
            .child(list().children(rows))
            .when(detail.items.is_empty(), |d| {
                d.child(div().child(format!("No {} found.", label(Some(tab)).to_lowercase())))
            })
            .into_any_element()
    }

    fn channels(&self, loaded: &Detail, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let rows = loaded.channels.iter().enumerate().map(|(index, channel)| {
            let id = channel.id.clone();
            row(c).child(
                link(("project-channel", index), channel.name.clone(), false, c)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_channel(id.clone(), cx))),
            )
        });
        list()
            .children(rows)
            .when(loaded.channels.is_empty(), |d| {
                d.child(row(c).child("No accessible linked channels."))
            })
            .into_any_element()
    }

    fn item(
        &self,
        loaded: &Detail,
        item: &buzz_relay_client::Verified,
        base: Option<&EntityRoute>,
        community: &gpui_kit::Entity<Community>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = &loaded.detail;
        let lookup = detail.commit.as_ref().zip(base).map(|(commit, base)| {
            let route = EntityRoute {
                kind: RouteKind::Repo,
                ..base.clone()
            }
            .with_commit(commit);
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .child("Look up commit in the base repository: ")
                .child(
                    link("project-commit-lookup", commit.clone(), false, c)
                        .text_mono()
                        .on_click(cx.listener(move |this, _, _, cx| this.go(route.clone(), cx))),
                )
                .child(
                    "Exact PR diffs from external forks are unavailable. This lookup only works if the commit is readable from the base repository.",
                )
        });
        let comments = detail.activity.iter().map(|event| {
            div()
                .py(rems(1.))
                .border_t_1()
                .border_color(c.border_standard)
                .child(secondary(
                    format!(
                        "{} · {}",
                        format_public_key(&event.pubkey.to_hex()),
                        locale_time(event.created_at.as_secs())
                    ),
                    c,
                ))
                .child(body(&event.content, community.read(cx), cx))
        });
        div()
            .flex()
            .flex_col()
            .child(div().child(format!(
                "{} · {} · {}",
                detail.status.unwrap_or("Open"),
                format_public_key(&item.pubkey.to_hex()),
                locale_time(item.created_at.as_secs())
            )))
            .child(body(&item.content, community.read(cx), cx))
            .children(lookup)
            .child(div().text_heading().child("Discussion"))
            .children(comments)
            .when(detail.activity.is_empty(), |d| {
                d.child(div().child("No comments."))
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn git_content(
        &self,
        git: &GitSnapshot,
        tab: EntityTab,
        route: &EntityRoute,
        detail: &EntityDetail,
        window: &Window,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match tab {
            EntityTab::Files => match &git.file {
                Some(file) => div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(
                        link("project-all-files", "All files", false, c)
                            .on_click(cx.listener(|this, _, _, cx| this.select_file(None, cx))),
                    )
                    .child(div().text_heading().child(file.path.clone()))
                    .child(match &file.content {
                        Some(content) => code("file", content, window, c),
                        None => div()
                            .child(format!(
                                "Binary file or text larger than 1 MiB ({} bytes).",
                                file.size
                            ))
                            .into_any_element(),
                    })
                    .into_any_element(),
                None => {
                    let rows = git.files.iter().enumerate().map(|(index, file)| {
                        let path = file.path.clone();
                        row(c)
                            .child(Icon::new(IconName::FileText).size(18.))
                            .child(
                                link(("project-file", index), file.path.clone(), false, c)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_file(Some(path.clone()), cx)
                                    })),
                            )
                            .child(small(format!("{} bytes", file.size), c))
                    });
                    list()
                        .children(rows)
                        .when(git.files.is_empty(), |d| d.child(row(c).child("No files.")))
                        .into_any_element()
                }
            },
            EntityTab::Commits => match &git.diff {
                Some(diff) => div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_heading()
                            .child(format!("Commit {}", git.head.clone().unwrap_or_default())),
                    )
                    .child(code("diff", diff, window, c))
                    .into_any_element(),
                None => {
                    let repo = detail
                        .repositories
                        .iter()
                        .find(|repo| repo.dtag == route.dtag)
                        .or(detail.repositories.first());
                    let rows = git.commits.iter().enumerate().map(|(index, commit)| {
                        let target = repo.and_then(|repo| {
                            EntityRoute::entity(EntityKind::Repo, &repo.owner, &repo.dtag)
                                .map(|r| r.with_commit(&commit.hash))
                        });
                        row(c)
                            .child(
                                link(("project-commit", index), commit.subject.clone(), false, c)
                                    .when_some(target, |d, target| {
                                        d.on_click(cx.listener(move |this, _, _, cx| {
                                            this.go(target.clone(), cx)
                                        }))
                                    }),
                            )
                            .child(small(
                                format!(
                                    "{} · {}",
                                    commit.author,
                                    commit.hash.get(..10).unwrap_or(&commit.hash)
                                ),
                                c,
                            ))
                    });
                    div()
                        .child(secondary(
                            format!("Latest {} commits", git.commits.len()),
                            c,
                        ))
                        .child(list().children(rows))
                        .into_any_element()
                }
            },
            _ => {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for commit in &git.commits {
                    match counts.iter_mut().find(|(name, _)| *name == commit.author) {
                        Some((_, count)) => *count += 1,
                        None => counts.push((commit.author.clone(), 1)),
                    }
                }
                counts.sort_by_key(|a| std::cmp::Reverse(a.1));
                div()
                    .child(secondary(
                        format!("Contributors in the latest {} commits", git.commits.len()),
                        c,
                    ))
                    .child(list().children(counts.into_iter().map(|(name, count)| {
                        row(c)
                            .child(name)
                            .child(small(format!("{count} commits"), c))
                    })))
                    .into_any_element()
            }
        }
    }
}

/// The repository a project's content tabs show: the one named like the
/// route, else the first.
fn primary(detail: &EntityDetail) -> Option<&Announcement> {
    detail
        .repositories
        .iter()
        .find(|repo| repo.dtag == detail.entity.dtag)
        .or(detail.repositories.first())
}

/// The repository's Git channel left the viewer's list after the read.
fn revoked(detail: &Detail, community: &gpui_kit::Entity<Community>, cx: &App) -> bool {
    detail.git_channel_listed
        && detail.git_channel.as_ref().is_some_and(|id| {
            !community
                .read(cx)
                .snapshot
                .channels
                .iter()
                .any(|c| c.id == *id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn tabs_are_labelled_in_buzz_app_order() {
        let labels: Vec<&str> = TABS.iter().map(|(_, label)| *label).collect();
        assert_eq!(
            labels,
            [
                "Overview",
                "Files",
                "Commits",
                "Pull requests",
                "Issues",
                "Contributors",
                "Channels"
            ]
        );
        assert_eq!(label(Some(EntityTab::Prs)).to_lowercase(), "pull requests");
    }

    #[test]
    fn identifiers_outside_the_link_grammar_cannot_be_opened() {
        use buzz_relay_client::projects::entity_dtag;
        assert!(entity_dtag("repo"));
        assert!(!entity_dtag("nested:repository"));
    }
}
