//! The app's read-only view of an agent's memory (buzz-app's
//! `MemoryPanel.tsx`). Every agent type keeps its memory the same way,
//! encrypted to the owner, so the app shows it once: `core` (what each
//! run is given) first, then the slugs the agent reads on demand. It is
//! read through the owner's session for the agent's community.

use crate::agents2::Agents2;
use crate::community::{AppState, Community};
use crate::ui::notice;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::{
    MemoryEntry, MemoryError, MemoryListing, RelayOrigin, Session, SessionStatus,
};
use buzz_ui::{
    ActiveTheme, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem, MenuLabel, MenuSize,
    Spinner, Tooltip, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::base::text::{
    MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView, TextViewStyle,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use markdown::mdast;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DESCRIPTION: &str = "These memories help this agent maintain context. Core memories are always available; other memories are recalled as needed. Only the agent can update them.";
const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
const MONTH_SECONDS: u64 = 30 * 24 * 60 * 60;
const TWO_MONTHS_SECONDS: u64 = 2 * MONTH_SECONDS;
const SPIN: Duration = Duration::from_secs(1);

/// What the panel shows (`MemorySnapshot["status"]`).
enum Status {
    Loading,
    Ready(MemoryListing),
    /// The session is not connected yet.
    Blocked,
    Error,
    Denied,
    Unavailable,
}

impl Status {
    fn message(&self) -> &'static str {
        match self {
            Self::Loading => "Loading memory…",
            Self::Blocked => "Memory can be read once Buzz is connected.",
            Self::Error => "Memory couldn't be read.",
            Self::Denied => "This community doesn't let you read this agent's memory.",
            Self::Unavailable | Self::Ready(_) => "Memory isn't available here.",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemorySort {
    #[default]
    Alphabetical,
    Chronological,
}

/// `memorySorter`: slugs in collation order, or newest first.
pub fn sort_memories(entries: &mut [&MemoryEntry], sort: MemorySort) {
    entries.sort_by(|left, right| match sort {
        MemorySort::Alphabetical => collate(&left.slug, &right.slug),
        MemorySort::Chronological => right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| collate(&left.slug, &right.slug)),
    });
}

/// `localeCompare` for memory slugs (lowercase letters, digits, `-`, `_`
/// and `/`): punctuation before digits before letters, as Unicode
/// collation orders them.
fn collate(left: &str, right: &str) -> Ordering {
    fn rank(byte: u8) -> u16 {
        match byte {
            b'_' => 0,
            b'-' => 1,
            b'/' => 2,
            b'0'..=b'9' => 3 + u16::from(byte - b'0'),
            b'a'..=b'z' | b'A'..=b'Z' => 13 + u16::from(byte.to_ascii_lowercase() - b'a'),
            other => 64 + u16::from(other),
        }
    }
    left.bytes().map(rank).cmp(right.bytes().map(rank))
}

/// `memoryTimestamp`: the relative ladder, with the first month in weeks
/// and the second as one month, then the date.
pub fn memory_timestamp(created_at: u64, now: u64) -> String {
    let age = now.saturating_sub(created_at);
    let ago =
        |value: u64, unit: &str| format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" });
    match age {
        0..60 => "just now".into(),
        60..3600 => ago(age / 60, "minute"),
        3600..86_400 => ago(age / 3600, "hour"),
        86_400..WEEK_SECONDS => ago(age / 86_400, "day"),
        WEEK_SECONDS..MONTH_SECONDS => ago(age / WEEK_SECONDS, "week"),
        MONTH_SECONDS..TWO_MONTHS_SECONDS => "1 month ago".into(),
        _ => crate::ui::time::short_month_day(created_at),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

pub struct MemoryPanel {
    pubkey: String,
    /// The session read from; a new one drops what the old one showed.
    session: Option<Arc<Session>>,
    status: Status,
    refreshing: bool,
    sort: MemorySort,
    sort_menu: bool,
    /// Expanded slugs, and whether each renders as Markdown (decided
    /// once, as it opens).
    open: HashMap<String, bool>,
    /// The slug whose action menu is open.
    actions: Option<String>,
    /// A right-clicked memory and where.
    context: Option<(String, Point<Pixels>)>,
    generation: u64,
    _read: Option<Task<()>>,
    _community: Option<(EntityId, Subscription)>,
    _observe: Vec<Subscription>,
}

impl MemoryPanel {
    pub fn new(pubkey: String, cx: &mut Context<Self>) -> Self {
        let mut observe = vec![cx.observe_global::<AppState>(|this, cx| this.sync(cx))];
        // The agent's community is known once Agents2 lists it.
        observe.extend(
            Agents2::global(cx).map(|service| cx.observe(&service, |this, _, cx| this.sync(cx))),
        );
        let mut this = Self {
            pubkey,
            session: None,
            status: Status::Unavailable,
            refreshing: false,
            sort: MemorySort::default(),
            sort_menu: false,
            open: HashMap::new(),
            actions: None,
            context: None,
            generation: 0,
            _read: None,
            _community: None,
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    /// The agent's community, while the owner has it.
    fn community(&self, cx: &App) -> Option<Entity<Community>> {
        let agent = Agents2::global(cx)?.read(cx).find(&self.pubkey)?.clone();
        let origin = RelayOrigin::parse(&agent.relay).ok()?;
        cx.try_global::<AppState>()?
            .communities
            .iter()
            .find(|community| community.read(cx).origin == origin)
            .cloned()
    }

    /// Follows the agent's community: reads when its session is ready and
    /// forgets what another session read.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let community = self.community(cx);
        let id = community.as_ref().map(Entity::entity_id);
        if self._community.as_ref().map(|(id, _)| *id) != id {
            self._community = community.map(|community| {
                let id = community.entity_id();
                (id, cx.observe(&community, |this, _, cx| this.follow(cx)))
            });
        }
        self.follow(cx);
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(community) = self.community(cx) else {
            if self.session.take().is_some() || !matches!(self.status, Status::Unavailable) {
                self.forget(Status::Unavailable, cx);
            }
            return;
        };
        let (session, ready) = {
            let community = community.read(cx);
            (
                community.session.clone(),
                matches!(community.snapshot.status, SessionStatus::Ready),
            )
        };
        let same = self
            .session
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &session));
        if !same {
            self.session = Some(session);
            self.forget(Status::Blocked, cx);
            self.open.clear();
        }
        if ready && matches!(self.status, Status::Blocked) {
            self.refresh(cx);
        }
    }

    /// Drops the shown plaintext and any read in flight.
    fn forget(&mut self, status: Status, cx: &mut Context<Self>) {
        self.generation += 1;
        self._read = None;
        self.refreshing = false;
        self.status = status;
        self.actions = None;
        self.context = None;
        cx.notify();
    }

    fn ready(&self, cx: &App) -> bool {
        self.community(cx).is_some_and(|community| {
            matches!(community.read(cx).snapshot.status, SessionStatus::Ready)
        })
    }

    /// Reads again, keeping a shown listing until the answer comes.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if !self.ready(cx) {
            self.status = Status::Blocked;
            cx.notify();
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.refreshing = true;
        if !matches!(self.status, Status::Ready(_)) {
            self.status = Status::Loading;
        }
        let pubkey = self.pubkey.clone();
        self._read = Some(cx.spawn(async move |this, cx| {
            let result = session.agent_memories(&pubkey).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.refreshing = false;
                this.status = match result {
                    Ok(listing) => Status::Ready(listing),
                    Err(MemoryError::Denied) => Status::Denied,
                    Err(MemoryError::Failed(_)) => Status::Error,
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn dismiss(&self, cx: &mut Context<Self>, close: fn(&mut Self)) -> Dismiss {
        let this = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = this.update(cx, |this, cx| {
                close(this);
                cx.notify();
            });
        })
    }

    fn refresh_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let can = self.session.is_some()
            && !matches!(
                self.status,
                Status::Loading | Status::Denied | Status::Unavailable
            );
        if !can {
            return None;
        }
        let c = cx.colors();
        let mut button = IconButton::new(
            SharedString::from(format!("memory-refresh-{}", self.pubkey)),
            IconName::ArrowsClockwise,
        )
        .size(IconButtonSize::Xs)
        .tooltip("Refresh")
        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        if self.refreshing && !cx.reduce_motion() {
            button = button.child(
                svg()
                    .path(IconName::ArrowsClockwise.path())
                    .flex_none()
                    .size(px(12.))
                    .text_color(c.text_standard)
                    .with_animation(
                        "memory-refreshing",
                        Animation::new(SPIN).repeat(),
                        |svg, delta| {
                            svg.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    ),
            );
        }
        Some(button.into_any_element())
    }

    fn sort_menu(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: &'static str, sort: MemorySort| {
            MenuItem::new(id, label)
                .size(MenuSize::Compact)
                .checked(self.sort == sort)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sort = sort;
                    this.sort_menu = false;
                    cx.notify();
                }))
        };
        let menu = Menu::new()
            .size(MenuSize::Compact)
            .child(MenuLabel::new("Sort"))
            .child(item(
                "memory-sort-alphabetical",
                "Alphabetically",
                MemorySort::Alphabetical,
            ))
            .child(item(
                "memory-sort-chronological",
                "Chronologically",
                MemorySort::Chronological,
            ));
        let group = format!("memory-core-{}", self.pubkey);
        div()
            .relative()
            .when(!self.sort_menu, |this| {
                this.invisible().group_hover(group, |this| this.visible())
            })
            .child(
                IconButton::new(
                    SharedString::from(format!("memory-organize-{}", self.pubkey)),
                    IconName::DotsThree,
                )
                .size(IconButtonSize::Xs)
                .tooltip("Organize memories")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.sort_menu = !this.sort_menu;
                    cx.notify();
                })),
            )
            .when(self.sort_menu, |this| {
                this.child(overlay::dropdown(
                    window,
                    Align::End,
                    menu,
                    self.dismiss(cx, |this| this.sort_menu = false),
                ))
            })
            .into_any_element()
    }

    fn section(
        &self,
        title: &'static str,
        entries: Vec<&MemoryEntry>,
        empty: &'static str,
        action: Option<AnyElement>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = cx.colors();
        let mut ordered = entries;
        sort_memories(&mut ordered, self.sort);
        let chronological = self.sort == MemorySort::Chronological;
        let list = if ordered.is_empty() {
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(empty)
                .into_any_element()
        } else {
            let now = now();
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .children(
                    ordered
                        .into_iter()
                        .map(|entry| self.card(entry, chronological, now, window, cx)),
                )
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .child(
                div()
                    .group(format!("memory-{}-{}", title_key(title), self.pubkey))
                    .flex()
                    .min_h(space::S8)
                    .items_center()
                    .justify_between()
                    .gap(space::S2)
                    .child(div().text_label().child(title))
                    .children(action),
            )
            .child(
                div()
                    .when(self.refreshing, |this| this.opacity(0.5))
                    .child(list),
            )
    }

    fn card(
        &self,
        entry: &MemoryEntry,
        show_timestamp: bool,
        now: u64,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let slug = entry.slug.clone();
        let key = format!("{}-{slug}", self.pubkey);
        let row = format!("memory-row-{key}");
        let rich = self.open.get(&slug).copied();
        let open = rich.is_some();
        let menu_open = self.actions.as_deref() == Some(slug.as_str());
        let title = match slug.strip_prefix("mem/") {
            Some(rest) => div()
                .flex()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .text_color(c.text_subtle)
                        .font_weight(FontWeight::NORMAL)
                        .child("mem/"),
                )
                .child(div().min_w_0().truncate().child(rest.to_owned())),
            None => div().min_w_0().truncate().child(slug.clone()),
        };
        let toggle = slug.clone();
        let trigger = div()
            .id(SharedString::from(format!("memory-trigger-{key}")))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(space::S2)
            .p(space::S4)
            .when(!show_timestamp, |this| this.pr(rems(3.)))
            .cursor_pointer()
            .text_body_sm()
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.open.remove(&toggle).is_none()
                    && let Status::Ready(listing) = &this.status
                    && let Some(entry) = listing.entries.iter().find(|e| e.slug == toggle)
                {
                    let rich = buzz_relay_client::fold::message_markdown(&entry.body).is_some();
                    this.open.insert(toggle.clone(), rich);
                }
                cx.notify();
            }))
            .child(
                Icon::new(IconName::CaretRight)
                    .size(16.)
                    .when(open, |icon| {
                        icon.rotate(radians(std::f32::consts::FRAC_PI_2))
                    }),
            )
            .child(div().flex_1().min_w_0().text_label().child(title))
            .when(show_timestamp, |this| {
                let full = crate::ui::time::full(entry.created_at);
                this.child(
                    div()
                        .id(SharedString::from(format!("memory-time-{key}")))
                        .flex_none()
                        .text_color(c.text_subtle)
                        .tooltip(Tooltip::text(full))
                        .group_hover(row.clone(), |this| this.invisible())
                        .when(menu_open, |this| this.invisible())
                        .child(memory_timestamp(entry.created_at, now)),
                )
            });
        let actions_slug = slug.clone();
        let actions = div()
            .absolute()
            .right(space::S4)
            .top_0()
            .bottom_0()
            .flex()
            .items_center()
            .when(!menu_open, |this| {
                this.invisible()
                    .group_hover(row.clone(), |this| this.visible())
            })
            .child(
                div()
                    .relative()
                    .child(
                        IconButton::new(
                            SharedString::from(format!("memory-actions-{key}")),
                            IconName::DotsThree,
                        )
                        .tooltip(format!("Actions for {slug}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.actions = match this.actions.take() {
                                Some(open) if open == actions_slug => None,
                                _ => Some(actions_slug.clone()),
                            };
                            cx.notify();
                        })),
                    )
                    .when(menu_open, |this| {
                        this.child(overlay::dropdown(
                            window,
                            Align::End,
                            copy_menu(entry, cx),
                            self.dismiss(cx, |this| this.actions = None),
                        ))
                    }),
            );
        let context_slug = slug.clone();
        div()
            .id(SharedString::from(format!("memory-{key}")))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(radius::CONTROL)
            .bg(c.surface_inset)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.context = Some((context_slug.clone(), event.position));
                    cx.notify();
                }),
            )
            .child(
                div()
                    .group(row)
                    .relative()
                    .flex()
                    .items_center()
                    .child(trigger)
                    .child(actions),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .px(space::S4)
                        .pb(space::S4)
                        .text_body_sm()
                        .child(memory_body(&key, &entry.body, rich == Some(true), cx)),
                )
            })
            .into_any_element()
    }

    fn context_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (slug, at) = self.context.clone()?;
        let Status::Ready(listing) = &self.status else {
            return None;
        };
        let entry = listing.entries.iter().find(|entry| entry.slug == slug)?;
        Some(
            overlay::context_menu(
                window,
                at,
                copy_menu(entry, cx),
                self.dismiss(cx, |this| this.context = None),
            )
            .into_any_element(),
        )
    }
}

fn title_key(title: &str) -> &'static str {
    if title == "Core memories" {
        "core"
    } else {
        "rest"
    }
}

/// "Copy name" and "Copy full body", each confirmed with a toast.
fn copy_menu(entry: &MemoryEntry, cx: &mut Context<MemoryPanel>) -> Menu {
    let copy = |value: String, label: &'static str| {
        cx.listener(move |this, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
            this.actions = None;
            this.context = None;
            notice::success(format!("Memory {label} copied."), cx);
            cx.notify();
        })
    };
    Menu::new()
        .size(MenuSize::Compact)
        .child(
            MenuItem::new("memory-copy-name", "Copy name")
                .size(MenuSize::Compact)
                .on_click(copy(entry.slug.clone(), "name")),
        )
        .child(
            MenuItem::new("memory-copy-body", "Copy full body")
                .size(MenuSize::Compact)
                .on_click(copy(entry.body.clone(), "body")),
        )
}

/// The body as Markdown, or as written past the size or depth limits.
fn memory_body(key: &str, body: &str, rich: bool, cx: &App) -> AnyElement {
    let c = cx.colors();
    if !rich {
        return div()
            .w_full()
            .min_w_0()
            .child(body.to_owned())
            .into_any_element();
    }
    let style = TextViewStyle::default()
        .with_foreground(c.text_standard)
        .with_muted_foreground(c.text_subtle)
        .with_link(c.text_link)
        .with_border(c.border_standard)
        .with_code_background(c.affordance_subtle)
        .with_inline_code(HighlightStyle {
            background_color: Some(c.affordance_subtle),
            ..Default::default()
        })
        .with_selection(c.text_link.opacity(0.25));
    TextView::markdown(
        SharedString::from(format!("memory-body-{key}")),
        SharedString::from(body.to_owned()),
    )
    .style(style)
    .markdown_extensions(MarkdownExtensions::default().plugin(AltText))
    .w_full()
    .into_any_element()
}

/// Memories are private context: their images show as alt text, so
/// opening one fetches nothing.
#[derive(Clone)]
struct AltText;

impl MarkdownPlugin for AltText {
    fn name(&self) -> &str {
        "memory-image"
    }

    fn parse(&self, node: &mdast::Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let alt = match node {
            mdast::Node::Image(image) => image.alt.clone(),
            mdast::Node::ImageReference(image) => image.alt.clone(),
            _ => return None,
        };
        Some(MarkdownNode::new("memory-image", ()).text(alt))
    }
}

impl Render for MemoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let description = div()
            .text_body_sm()
            .text_color(c.text_subtle)
            .child(DESCRIPTION);
        let body: AnyElement = match &self.status {
            Status::Ready(listing) => {
                let core: Vec<&MemoryEntry> = listing
                    .entries
                    .iter()
                    .filter(|e| e.slug == "core")
                    .collect();
                let rest: Vec<&MemoryEntry> = listing
                    .entries
                    .iter()
                    .filter(|e| e.slug != "core")
                    .collect();
                let partial = listing.partial;
                let action = div()
                    .flex()
                    .items_center()
                    .gap(space::S1)
                    .children(self.refresh_button(cx))
                    .child(self.sort_menu(window, cx))
                    .into_any_element();
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S6)
                    .child(self.section(
                        "Core memories",
                        core,
                        "No core memory yet. The agent creates it when it has something to keep.",
                        Some(action),
                        window,
                        cx,
                    ))
                    .child(self.section("Memories", rest, "No memories yet.", None, window, cx))
                    .when(partial, |this| {
                        this.child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child("Some memories couldn't be shown."),
                        )
                    })
                    .into_any_element()
            }
            Status::Loading => div()
                .flex()
                .min_h(rems(3.))
                .items_center()
                .justify_center()
                .text_color(c.text_subtle)
                .child(Spinner::new())
                .into_any_element(),
            status => {
                let error = matches!(status, Status::Error);
                let message = status.message();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S2)
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(if error { c.text_danger } else { c.text_subtle })
                            .child(message),
                    )
                    .children(self.refresh_button(cx))
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .child(description)
            .child(body)
            .children(self.context_menu(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn entry(slug: &str, created_at: u64) -> MemoryEntry {
        MemoryEntry {
            slug: slug.into(),
            body: String::new(),
            event_id: "a".repeat(64),
            created_at,
        }
    }

    #[test]
    fn sorts_memories_alphabetically_or_chronologically() {
        let entries = [
            entry("mem/alpha", 100),
            entry("mem/zulu", 300),
            entry("mem/middle", 200),
            entry("mem/a-b", 200),
            entry("mem/a_b", 200),
            entry("mem/a1", 200),
        ];
        let order = |sort| {
            let mut refs: Vec<&MemoryEntry> = entries.iter().collect();
            sort_memories(&mut refs, sort);
            refs.iter().map(|e| e.slug.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(
            order(MemorySort::Alphabetical),
            [
                "mem/a_b",
                "mem/a-b",
                "mem/a1",
                "mem/alpha",
                "mem/middle",
                "mem/zulu"
            ]
        );
        assert_eq!(
            order(MemorySort::Chronological),
            [
                "mem/zulu",
                "mem/a_b",
                "mem/a-b",
                "mem/a1",
                "mem/middle",
                "mem/alpha"
            ]
        );
    }

    #[test]
    fn timestamps_count_weeks_then_one_month() {
        let now = 100 * MONTH_SECONDS;
        assert_eq!(memory_timestamp(now - 30, now), "just now");
        assert_eq!(memory_timestamp(now - 120, now), "2 minutes ago");
        assert_eq!(memory_timestamp(now - 3600, now), "1 hour ago");
        assert_eq!(memory_timestamp(now - 2 * 86_400, now), "2 days ago");
        assert_eq!(memory_timestamp(now - WEEK_SECONDS, now), "1 week ago");
        assert_eq!(memory_timestamp(now - 3 * WEEK_SECONDS, now), "3 weeks ago");
        assert_eq!(memory_timestamp(now - MONTH_SECONDS, now), "1 month ago");
        assert_eq!(
            memory_timestamp(now - 2 * MONTH_SECONDS + 1, now),
            "1 month ago"
        );
        let old = now - 2 * MONTH_SECONDS;
        assert_eq!(
            memory_timestamp(old, now),
            crate::ui::time::short_month_day(old)
        );
        assert!(!memory_timestamp(old, now).starts_with("on "));
        // A clock behind the memory reads as now.
        assert_eq!(memory_timestamp(now + 60, now), "just now");
    }
}
