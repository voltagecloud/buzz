//! The composer's "Issues and pull requests" tool (`ResourcePicker.tsx`).
//!
//! Each composer gets one. It owns the channel's project lookup and the
//! item lists; choosing an item checks it still exists, then asks the
//! composer to insert a link to it at the caret. A channel with no
//! project shows no button.

use super::super::composer_tools::{self, ToolContext};
use super::super::search_kit::{Highlight, match_name, matched_label};
use buzz_relay_client::projects::{
    Entity as Announcement, EntityFailure, EntityKind, EntityRoute, EntityTab, Home, ItemKind,
    entity_dtag, entity_hex, values,
};
use buzz_ui::{
    ActiveTheme, Button, IconButton, IconButtonSize, IconName, MaterialStyled, SearchField,
    Surface, TypeStyled, tokens::radius,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::time::{Duration, Instant};

const LABEL: &str = "Add issue or pull request";

fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Issue => "Issue",
        ItemKind::Pr => "Pull request",
    }
}

enum HomeState {
    /// Checking, with the session's last answer for the channel, which the
    /// button keeps showing meanwhile.
    Loading(Option<Home>),
    None,
    Ambiguous,
    Error,
    Project(Announcement),
}

#[derive(Clone)]
struct Item {
    kind: ItemKind,
    id: String,
    label: String,
    created_at: u64,
    repository: Announcement,
}

enum Items {
    Loading,
    Error,
    Ready { items: Vec<Item>, truncated: bool },
}

struct Checking {
    id: String,
    failed: Option<String>,
}

pub struct ResourcePicker {
    context: ToolContext,
    home: HomeState,
    open: bool,
    /// When a press outside closed it, so that press on the button does
    /// not reopen it.
    closed: Option<Instant>,
    input: Entity<InputState>,
    items: Items,
    checking: Option<Checking>,
    rejected: Option<String>,
    highlight: Highlight,
    _home: Option<Task<()>>,
    _items: Option<Task<()>>,
    _check: Option<Task<()>>,
    _input: Subscription,
}

impl ResourcePicker {
    pub fn new(context: ToolContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut picker = Self {
            context,
            home: HomeState::Loading(None),
            open: false,
            closed: None,
            input,
            items: Items::Loading,
            checking: None,
            rejected: None,
            highlight: Highlight::new().highlight_empty(true).wrap(true),
            _home: None,
            _items: None,
            _check: None,
            _input: subscription,
        };
        picker.load_home(false, cx);
        picker
    }

    fn project(&self) -> Option<&Announcement> {
        match &self.home {
            HomeState::Project(project) => Some(project),
            _ => None,
        }
    }

    /// The composer cannot take an insertion now.
    fn disabled(&self, cx: &App) -> bool {
        self.context
            .composer
            .upgrade()
            .is_none_or(|composer| composer.read(cx).sending())
    }

    /// Checks the channel's project on every mount and Retry. Until it
    /// answers, a mount shows the session's last answer; a Retry does not.
    fn load_home(&mut self, retry: bool, cx: &mut Context<Self>) {
        let projects = self.context.community.read(cx).session.projects();
        let channel = self.context.channel.clone();
        self.home = HomeState::Loading((!retry).then(|| projects.peek_home(&channel)).flatten());
        self._home = Some(cx.spawn(async move |this, cx| {
            let home = projects.home(channel).await;
            let _ = this.update(cx, |this, cx| {
                this.home = match home {
                    Ok(Home::None) => HomeState::None,
                    Ok(Home::Ambiguous) => HomeState::Ambiguous,
                    Ok(Home::Project(project)) => HomeState::Project(project),
                    Err(_) => HomeState::Error,
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Reads the project's issues and pull requests, newest first. Every
    /// opening and retry reads again.
    fn load_items(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project().cloned() else {
            return;
        };
        self.items = Items::Loading;
        let projects = self.context.community.read(cx).session.projects();
        self._items = Some(cx.spawn(async move |this, cx| {
            let read = |kind: ItemKind| {
                let projects = projects.clone();
                let route = EntityRoute::entity(EntityKind::Project, &project.owner, &project.dtag)
                    .map(|route| {
                        route.with_tab(Some(match kind {
                            ItemKind::Pr => EntityTab::Prs,
                            ItemKind::Issue => EntityTab::Issues,
                        }))
                    });
                async move {
                    let route = route.ok_or(EntityFailure::InvalidTarget)?;
                    let detail = projects.load(route).await?;
                    let items: Vec<Item> = detail
                        .items
                        .iter()
                        .filter_map(|event| {
                            let repository = detail
                                .repositories
                                .iter()
                                .find(|repo| values(event, "a").contains(&repo.address.as_str()))?;
                            let id = event.id.to_hex();
                            (entity_dtag(&repository.dtag) && entity_hex(&id)).then(|| {
                                let label = buzz_relay_client::projects::value(event, "subject")
                                    .map(str::to_string)
                                    .or_else(|| {
                                        event
                                            .content
                                            .split('\n')
                                            .next()
                                            .filter(|line| !line.is_empty())
                                            .map(str::to_string)
                                    })
                                    .unwrap_or_else(|| kind_label(kind).to_string());
                                Item {
                                    kind,
                                    id,
                                    label,
                                    created_at: event.created_at.as_secs(),
                                    repository: repository.clone(),
                                }
                            })
                        })
                        .collect();
                    Ok::<_, EntityFailure>((items, detail.truncated))
                }
            };
            let (issues, prs) = futures::join!(read(ItemKind::Issue), read(ItemKind::Pr));
            let _ = this.update(cx, |this, cx| {
                this.items = match (issues, prs) {
                    (Ok((issues, a)), Ok((prs, b))) => {
                        let mut items: Vec<Item> = issues.into_iter().chain(prs).collect();
                        items.sort_by_key(|a| std::cmp::Reverse(a.created_at));
                        Items::Ready {
                            items,
                            truncated: a || b,
                        }
                    }
                    _ => Items::Error,
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open = open;
        self._check = None;
        self.checking = None;
        self.rejected = None;
        if open {
            self.load_items(cx);
            if self.project().is_some() {
                self.input.update(cx, |state, cx| state.focus(window, cx));
            }
        } else {
            self.closed = Some(Instant::now());
        }
        cx.notify();
    }

    /// The rows for the typed text: items whose title or repository
    /// contain it, best title match first (exact, prefix, word start, the
    /// rest), newest first within a rank.
    fn shown(&self, cx: &App) -> Vec<(Item, Vec<usize>)> {
        let Items::Ready { items, .. } = &self.items else {
            return Vec::new();
        };
        let query = self.input.read(cx).value().trim().to_lowercase();
        let mut shown: Vec<(u8, Item, Vec<usize>)> = items
            .iter()
            .filter(|item| {
                format!("{} {}", item.label, item.repository.name)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|item| {
                let found = (!query.is_empty())
                    .then(|| match_name(&item.label, &query))
                    .flatten();
                let rank = found.as_ref().map_or(u8::MAX, |found| found.rank);
                let positions = found
                    .filter(|found| found.rank <= 3)
                    .map(|found| found.positions)
                    .unwrap_or_default();
                (rank, item.clone(), positions)
            })
            .collect();
        shown.sort_by_key(|(rank, ..)| *rank);
        shown
            .into_iter()
            .map(|(_, item, positions)| (item, positions))
            .collect()
    }

    /// Checks the item still exists, so a deleted or retargeted one never
    /// becomes a link, then inserts it.
    fn choose(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let Some(route) = EntityRoute::item(
            item.kind,
            &item.repository.owner,
            &item.repository.dtag,
            &item.id,
        ) else {
            return;
        };
        self.input.update(cx, |state, cx| state.focus(window, cx));
        self.checking = Some(Checking {
            id: item.id.clone(),
            failed: None,
        });
        self.rejected = None;
        let projects = self.context.community.read(cx).session.projects();
        self._check = Some(cx.spawn_in(window, async move |this, cx| {
            let checked = projects.load(route.clone()).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match checked {
                    Ok(_) => {
                        this.checking = None;
                        let inserted = this.context.composer.upgrade().map(|composer| {
                            composer_tools::insert_resource(
                                &composer,
                                &route.href(),
                                &item.label,
                                window,
                                cx,
                            )
                        });
                        match inserted {
                            Some(Ok(())) => {
                                this.open = false;
                            }
                            Some(Err(reason)) => this.rejected = Some(reason.to_string()),
                            None => {}
                        }
                    }
                    Err(failure) => {
                        this.checking = Some(Checking {
                            id: item.id.clone(),
                            failed: Some(if failure == EntityFailure::NotFound {
                                format!(
                                    "This {} is no longer available.",
                                    kind_label(item.kind).to_lowercase()
                                )
                            } else {
                                "Could not check this item. Choose it again to retry.".into()
                            }),
                        });
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn popup(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let caption = |text: String| div().text_caption().text_color(c.text_subtle).child(text);
        let mut content = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .p(rems(0.75))
            .gap(rems(0.5));
        if self.project().is_none() {
            let text = match self.home {
                HomeState::Ambiguous => {
                    "This channel belongs to more than one project, so issues and pull requests can't be chosen here."
                }
                _ => "Could not load this channel's project.",
            };
            return content
                .child(caption(text.into()))
                .child(
                    div().flex().child(
                        Button::new("resource-retry-project", "Retry project")
                            .on_click(cx.listener(|this, _, _, cx| this.load_home(true, cx))),
                    ),
                )
                .into_any_element();
        };
        let busy = self
            .checking
            .as_ref()
            .is_some_and(|check| check.failed.is_none());
        let disabled = self.disabled(cx);
        let shown = self.shown(cx);
        let keys: Vec<String> = if disabled || busy {
            Vec::new()
        } else {
            shown.iter().map(|(item, _)| item.id.clone()).collect()
        };
        let query = self.input.read(cx).value().to_string();
        self.highlight.sync(&query, &keys);
        self.highlight
            .reveal(|key| keys.iter().position(|k| k == key));
        let active = self.highlight.active().map(str::to_string);
        // buzz-app's "Search <project>" label is for assistive technology
        // only; the field shows "Search".
        content = content.child(SearchField::new(&self.input).capsule().disabled(disabled));
        match &self.items {
            Items::Loading => {
                content = content.child(caption("Loading issues and pull requests…".into()))
            }
            Items::Error => {
                content = content
                    .child(caption("Could not load issues and pull requests.".into()))
                    .child(
                        div().flex().child(
                            Button::new("resource-retry", "Retry")
                                .on_click(cx.listener(|this, _, _, cx| this.load_items(cx))),
                        ),
                    )
            }
            Items::Ready {
                truncated: true, ..
            } => {
                content = content.child(caption(
                    "Showing the latest items only. Some issues or pull requests may be missing."
                        .into(),
                ))
            }
            Items::Ready { .. } => {}
        }
        if let Some(message) = self
            .checking
            .as_ref()
            .and_then(|check| check.failed.clone())
            .or_else(|| self.rejected.clone())
        {
            content = content.child(caption(message));
        }
        let ix = c.interaction(Surface::Floating);
        let checking = self
            .checking
            .as_ref()
            .filter(|check| check.failed.is_none())
            .map(|check| check.id.clone());
        let row_keys = keys.clone();
        let rows = shown.iter().map(|(item, positions)| {
            let selected = active.as_deref() == Some(item.id.as_str());
            let hover_key = item.id.clone();
            let hover_keys = row_keys.clone();
            let chosen = item.clone();
            let detail = if checking.as_deref() == Some(item.id.as_str()) {
                "Checking…".to_string()
            } else {
                format!("{} · {}", kind_label(item.kind), item.repository.name)
            };
            let inactive = disabled || busy;
            div()
                .id(SharedString::from(format!("resource-{}", item.id)))
                .flex()
                .flex_col()
                .min_h(rems(2.))
                .p(rems(0.5))
                .rounded(radius::ROW)
                .text_body()
                .text_color(if inactive {
                    c.text_unavailable
                } else {
                    c.text_standard
                })
                .when(selected, |d| d.bg(ix.selected))
                .when(!inactive, |d| {
                    d.cursor_pointer()
                        .when(!selected, |d| d.hover(|s| s.bg(ix.row_hover)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(chosen.clone(), window, cx)
                        }))
                })
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if this
                        .highlight
                        .hover(&hover_key, event.position, &hover_keys)
                    {
                        cx.notify();
                    }
                }))
                .child(div().child(matched_label(item.label.clone(), positions)))
                .child(div().text_caption().text_color(c.text_subtle).child(detail))
        });
        let empty = match &self.items {
            Items::Ready { items, .. } if shown.is_empty() => Some(if items.is_empty() {
                "This project has no issues or pull requests yet."
            } else {
                "No matching issues or pull requests."
            }),
            _ => None,
        };
        content
            .child(
                div()
                    .id("resource-choices")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(rems(0.25))
                    .overflow_y_scroll()
                    .track_scroll(self.highlight.scroll_handle())
                    .children(rows)
                    .children(empty.map(|text| caption(text.into()))),
            )
            .into_any_element()
    }
}

impl Render for ResourcePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        // While checking, the session's last answer decides: none known or
        // no project hides the button, so it does not flash on a switch; a
        // known project shows it disabled.
        let pending = matches!(self.home, HomeState::Loading(_));
        let (hidden, unavailable) = match &self.home {
            HomeState::Loading(cached) => (
                matches!(cached, None | Some(Home::None)),
                matches!(cached, Some(Home::Ambiguous)),
            ),
            HomeState::None => (true, false),
            HomeState::Ambiguous | HomeState::Error => (false, true),
            HomeState::Project(_) => (false, false),
        };
        if hidden {
            return div().into_any_element();
        }
        let disabled = self.disabled(cx);
        if disabled && self.open {
            // A hidden choice must not insert after the composer is
            // re-enabled.
            self.open = false;
            self._check = None;
            self.checking = None;
            self.rejected = None;
        }
        let tooltip = if pending {
            "Loading project…".to_string()
        } else if self.project().is_some() {
            LABEL.to_string()
        } else {
            format!("{LABEL} (unavailable)")
        };
        let open = self.open && !disabled && !pending;
        let popup = open.then(|| {
            let content = self.popup(cx);
            div().absolute().left_0().top_0().child(deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(0.), px(-4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("resource-picker")
                            .occlude()
                            .w(px(380.))
                            .h(px(360.))
                            .overflow_hidden()
                            .floating_surface(&c)
                            .on_mouse_down_out(
                                cx.listener(|this, _, window, cx| this.set_open(false, window, cx)),
                            )
                            .capture_key_down(cx.listener(
                                |this, event: &KeyDownEvent, window, cx| {
                                    let keys: Vec<String> = this
                                        .shown(cx)
                                        .iter()
                                        .map(|(item, _)| item.id.clone())
                                        .collect();
                                    let keystroke = &event.keystroke;
                                    if keystroke.modifiers.shift {
                                        return;
                                    }
                                    match keystroke.key.as_str() {
                                        "down" => {
                                            this.highlight.step(true, &keys);
                                        }
                                        "up" => {
                                            this.highlight.step(false, &keys);
                                        }
                                        "enter" => {
                                            if let Some(key) = this.highlight.chosen(&keys)
                                                && this
                                                    .checking
                                                    .as_ref()
                                                    .is_none_or(|check| check.failed.is_some())
                                                && let Some((item, _)) = this
                                                    .shown(cx)
                                                    .into_iter()
                                                    .find(|(item, _)| item.id == key)
                                            {
                                                this.choose(item, window, cx);
                                            }
                                        }
                                        "escape" => {
                                            this.set_open(false, window, cx);
                                            if let Some(composer) = this.context.composer.upgrade()
                                            {
                                                composer.update(cx, |composer, cx| {
                                                    composer.focus(window, cx)
                                                });
                                            }
                                        }
                                        _ => return,
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                },
                            ))
                            .child(content),
                    ),
            ))
        });
        div()
            .relative()
            .flex_none()
            .child(
                IconButton::new("resource-picker-trigger", IconName::GitPullRequest)
                    .size(IconButtonSize::Sm)
                    .selected(open)
                    .disabled(disabled || pending)
                    .tooltip(tooltip)
                    .when(unavailable, |b| b.icon_color(c.text_warning))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this
                            .closed
                            .take()
                            .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
                        {
                            return;
                        }
                        let open = !this.open;
                        this.set_open(open, window, cx);
                    })),
            )
            .children(popup)
            .into_any_element()
    }
}
