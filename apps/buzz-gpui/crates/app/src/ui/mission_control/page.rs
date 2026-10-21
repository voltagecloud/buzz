//! The page: a summary of each host, filters, then every agent with the
//! ones needing attention first.

use super::row::AgentRow;
use crate::hosts::Hosts;
use crate::mission_control::Fleet;
use crate::mission_control::model::{Board, HostSummary, Key, Link, Status};
use crate::ui::agents2::empty_state;
use crate::ui::settings::OpenHostSettings;
use buzz_ui::{
    ActiveTheme, Badge, BadgeTone, Button, ButtonSize, Colors, Icon, IconButton, IconButtonSize,
    IconName, PanelHeader, TypeStyled, tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Attention,
    Working,
    Offline,
}

/// What the page draws itself, apart from its rows' contents: a change
/// redraws the page, anything else only the rows that changed.
#[derive(PartialEq)]
struct Shape {
    hosts: Vec<HostSummary>,
    rows: Vec<(Key, Status, Link, bool, bool)>,
}

impl Shape {
    fn of(board: &Board) -> Self {
        Self {
            hosts: board.hosts.clone(),
            rows: board
                .entries
                .iter()
                .map(|e| {
                    (
                        e.key.clone(),
                        e.status,
                        e.link,
                        e.attention.is_some(),
                        e.doing.active,
                    )
                })
                .collect(),
        }
    }
}

pub struct MissionControlPage {
    fleet: Option<Entity<Fleet>>,
    board: Arc<Board>,
    rows: HashMap<Key, Entity<AgentRow>>,
    filter: Filter,
    /// Only this host's agents, when one is chosen.
    host: Option<u64>,
    scroll: ScrollHandle,
    _fleet: Option<Subscription>,
}

impl MissionControlPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let fleet = Fleet::global(cx);
        let subscription = fleet
            .as_ref()
            .map(|fleet| cx.observe(fleet, |this, _, cx| this.sync(cx)));
        let mut this = Self {
            fleet,
            board: Arc::default(),
            rows: HashMap::new(),
            filter: Filter::All,
            host: None,
            scroll: ScrollHandle::new(),
            _fleet: subscription,
        };
        this.sync(cx);
        this
    }

    /// Takes the newest board; rows whose agent changed redraw, the rest
    /// keep their last frame.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(fleet) = &self.fleet else {
            return;
        };
        let board = fleet.read(cx).board();
        if Arc::ptr_eq(&board, &self.board) {
            return;
        }
        self.rows
            .retain(|key, _| board.entries.iter().any(|entry| entry.key == *key));
        for entry in &board.entries {
            match self.rows.get(&entry.key) {
                Some(row) => row.update(cx, |row, cx| {
                    if row.entry != *entry {
                        row.entry = entry.clone();
                        cx.notify();
                    }
                }),
                None => {
                    let row = cx.new(|_| AgentRow::new(entry.clone()));
                    self.rows.insert(entry.key.clone(), row);
                }
            }
        }
        if self
            .host
            .is_some_and(|host| !board.hosts.iter().any(|h| h.key == host))
        {
            self.host = None;
        }
        let reshaped = Shape::of(&board) != Shape::of(&self.board);
        self.board = board;
        if reshaped {
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(agents) = crate::agents2::Agents2::global(cx) {
            agents.update(cx, |agents, cx| agents.refresh(cx).detach());
        }
        if let Some(fleet) = &self.fleet {
            fleet.update(cx, |fleet, cx| fleet.read_usage(cx));
        }
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        PanelHeader::new()
            .joined(true)
            .title("Mission control")
            .action(
                Button::new("mc-hosts", "Manage hosts")
                    .size(ButtonSize::Sm)
                    .icon(IconName::Plug)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenHostSettings), cx)
                    }),
            )
            .action(
                IconButton::new("mc-refresh", IconName::ArrowsClockwise)
                    .size(IconButtonSize::Sm)
                    .tooltip("Refresh every host")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn host_card(&self, host: &HostSummary, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.host == Some(host.key);
        let key = host.key;
        let live = host.link == Link::Connected;
        let tone = match host.link {
            Link::Connected => BadgeTone::Success,
            Link::Connecting => BadgeTone::Neutral,
            Link::Offline => BadgeTone::Danger,
            Link::Refused => BadgeTone::Warning,
        };
        let counts = match host.total {
            0 if live => "No agents".to_string(),
            0 => "No known agents".to_string(),
            total if live => format!(
                "{total} agent{} · {} working",
                if total == 1 { "" } else { "s" },
                host.working
            ),
            total => format!(
                "{total} agent{} last known",
                if total == 1 { "" } else { "s" }
            ),
        };
        let reconnect = (!live && host.link != Link::Connecting).then(|| {
            Button::new(
                SharedString::from(format!("mc-reconnect-{key}")),
                "Reconnect",
            )
            .size(ButtonSize::Sm)
            .ghost()
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(host) = cx
                    .try_global::<Hosts>()
                    .and_then(|hosts| hosts.hosts.iter().find(|h| h.entity_id().as_u64() == key))
                {
                    host.read(cx).reconnect();
                }
            }))
        });
        div()
            .id(SharedString::from(format!("mc-host-{key}")))
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .w(rems(19.5))
            .p(rems(0.875))
            .rounded(radius::CONTAINER)
            .border_1()
            .border_color(if selected {
                c.border_focus
            } else {
                c.border_standard
            })
            .bg(c.surface_panel)
            .cursor_pointer()
            .hover(|this| this.bg(c.affordance_panel_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.host = (this.host != Some(key)).then_some(key);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .min_w_0()
                    .child(
                        Icon::new(if host.address.is_some() {
                            IconName::Globe
                        } else {
                            IconName::Monitor
                        })
                        .size(16.)
                        .color(c.text_subtle),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_label_sm()
                            .truncate()
                            .child(host.label.clone()),
                    )
                    .child(Badge::new(host.link.label()).tone(tone)),
            )
            .child(
                div()
                    .text_caption()
                    .text_color(c.text_metadata)
                    .truncate()
                    .child(
                        host.address
                            .clone()
                            .unwrap_or_else(|| "Runs inside this app".into()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .min_h(rems(1.75))
                    .text_body_sm()
                    .text_color(if live { c.text_subtle } else { c.text_metadata })
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .gap(rems(0.375))
                            .child(div().truncate().child(counts))
                            .when(host.attention > 0, |this| {
                                this.child("·").child(
                                    div()
                                        .flex_none()
                                        .text_color(c.text_danger)
                                        .child(format!("{} need attention", host.attention)),
                                )
                            }),
                    )
                    .children(reconnect),
            )
            .children(host.detail.clone().filter(|_| !live).map(|detail| {
                div()
                    .text_caption()
                    .text_color(c.text_metadata)
                    .line_clamp(2)
                    .child(detail)
            }))
            .into_any_element()
    }

    fn filters(&self, c: &Colors, cx: &mut Context<Self>) -> Div {
        let entries = self
            .board
            .entries
            .iter()
            .filter(|e| self.host.is_none_or(|host| e.key.host == host));
        let (mut all, mut attention, mut working, mut offline) = (0, 0, 0, 0);
        for entry in entries {
            all += 1;
            attention += entry.attention.is_some() as usize;
            working += entry.doing.active as usize;
            offline += offline_entry(entry) as usize;
        }
        let chip = |filter: Filter, label: &str, count: usize, cx: &mut Context<Self>| {
            let selected = self.filter == filter;
            div()
                .id(SharedString::from(format!("mc-filter-{filter:?}")))
                .flex()
                .items_center()
                .gap(rems(0.375))
                .h(rems(1.75))
                .px(rems(0.75))
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    c.border_focus
                } else {
                    c.border_standard
                })
                .bg(if selected {
                    c.affordance_selected
                } else {
                    c.surface_panel
                })
                .text_body_sm()
                .cursor_pointer()
                .hover(|this| this.bg(c.affordance_panel_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    cx.notify();
                }))
                .child(label.to_string())
                .child(
                    div()
                        .text_caption()
                        .text_color(if filter == Filter::Attention && count > 0 {
                            c.text_danger
                        } else {
                            c.text_metadata
                        })
                        .child(count.to_string()),
                )
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rems(0.5))
            .child(chip(Filter::All, "All agents", all, cx))
            .child(chip(Filter::Attention, "Needs attention", attention, cx))
            .child(chip(Filter::Working, "Working", working, cx))
            .child(chip(Filter::Offline, "Offline", offline, cx))
    }

    fn list(&self, c: &Colors, cx: &App) -> AnyElement {
        let shown: Vec<_> = self
            .board
            .entries
            .iter()
            .filter(|e| self.host.is_none_or(|host| e.key.host == host))
            .filter(|e| match self.filter {
                Filter::All => true,
                Filter::Attention => e.attention.is_some(),
                Filter::Working => e.doing.active,
                Filter::Offline => offline_entry(e),
            })
            .collect();
        if shown.is_empty() {
            let (title, description) = match (self.filter, self.board.entries.is_empty()) {
                (_, true) => (
                    "No agents yet",
                    "Agents on this computer and on every host you add show here, live.",
                ),
                (Filter::Attention, _) => (
                    "All clear",
                    "No agent has failed, stalled or is waiting on you.",
                ),
                _ => ("Nothing here", "No agent matches this filter."),
            };
            return empty_state("◎", title, description, None, c).into_any_element();
        }
        let rows = shown.into_iter().enumerate().filter_map(|(ix, entry)| {
            let row = self.rows.get(&entry.key)?.clone();
            // Rows draw from their last frame unless they changed.
            let view: AnyElement = if row.read(cx).overlay_open() {
                row.into_any_element()
            } else {
                AnyView::from(row)
                    .cached(StyleRefinement::default().w_full().h(super::row::HEIGHT))
                    .into_any_element()
            };
            Some(
                div()
                    .w_full()
                    .when(ix > 0, |this| {
                        this.border_t_1().border_color(c.border_standard)
                    })
                    .child(view),
            )
        });
        div()
            .w_full()
            .rounded(radius::CONTAINER)
            .border_1()
            .border_color(c.border_standard)
            .bg(c.surface_panel)
            .overflow_hidden()
            .children(rows)
            .into_any_element()
    }
}

impl Render for MissionControlPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let board = self.board.clone();
        let attention = board.attention();
        let working = board.entries.iter().filter(|e| e.doing.active).count();
        let summary = match (attention, working) {
            (0, 0) => "Nothing needs you.".to_string(),
            (0, n) => format!("{n} at work. Nothing needs you."),
            (a, n) => format!(
                "{a} need{} attention · {n} at work",
                if a == 1 { "s" } else { "" }
            ),
        };
        let hosts: Vec<AnyElement> = board
            .hosts
            .iter()
            .map(|host| self.host_card(host, &c, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .text_color(c.text_standard)
            .child(self.header(cx))
            .child(
                div()
                    .id("mission-control")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p(rems(1.5))
                    .text_body()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(1.5))
                            .w_full()
                            .max_w(rems(72.))
                            .mx_auto()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.75))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(rems(0.75))
                                            .child(div().text_label().child("Hosts"))
                                            .child(
                                                div()
                                                    .text_body_sm()
                                                    .text_color(if attention > 0 {
                                                        c.text_danger
                                                    } else {
                                                        c.text_metadata
                                                    })
                                                    .child(summary),
                                            ),
                                    )
                                    .child(
                                        div().flex().flex_wrap().gap(rems(0.75)).children(hosts),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.75))
                                    .child(self.filters(&c, cx))
                                    .child(self.list(&c, cx)),
                            ),
                    ),
            )
    }
}

/// Its host or its community is not connected.
fn offline_entry(entry: &crate::mission_control::model::Entry) -> bool {
    entry.link != Link::Connected || entry.status == Status::Connecting
}
