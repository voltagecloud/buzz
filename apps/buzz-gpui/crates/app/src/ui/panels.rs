//! Panels beside a page (`features/panels`): the dock that holds them, a
//! panel card's chrome, drill-in subviews and the tabbed panel workspace
//! (ui.md §2.10).

use super::resize_handle::{OnCommit, OnResize, ResizeHandle};
use super::tab_drag::{self, Geometry, PaneKey, TabDrag};
use super::workspace::layout::{DockMode, Split};
use buzz_ui::tokens::{duration, easing, shadow};
use buzz_ui::{
    ActiveTheme, IconButton, IconButtonSize, IconName, PanelHeader, Tab, TabVariant, Tabs,
    TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub type Close = Rc<dyn Fn(&mut Window, &mut App)>;

/// Where a page's dock sits and how wide it is.
pub struct Dock {
    /// Names the dock's motion and handle.
    pub id: SharedString,
    pub split: Split,
    /// The width the user dragged to, kept while the dock is closed.
    pub preferred: Option<f32>,
    /// The window's width, for the overlay breakpoints.
    pub window: f32,
    /// The page region's width.
    pub available: f32,
    /// Changes with each opening, so a new opening slides in.
    pub opening: u64,
    pub on_resize: OnResize,
    pub on_commit: OnCommit,
}

impl Dock {
    /// The page beside its dock: side by side behind a resize handle on a
    /// wide window, floating over the page's right edge at 1000px and
    /// below, and covering it at 650px and below.
    pub fn render(self, page: AnyElement, dock: Option<AnyElement>, cx: &App) -> AnyElement {
        let c = *cx.colors();
        let Some(dock) = dock else {
            return div()
                .flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(page)
                .into_any_element();
        };
        let mode = self.split.mode(self.window, self.available, self.preferred);
        let reduce = cx.reduce_motion();
        // Desktop docks travel 12px as they open; overlays their width.
        let travel = match mode {
            DockMode::Split { .. } => 12.,
            DockMode::Overlay { width } => width,
            DockMode::Full => self.available,
        };
        let enter = Animation::new(duration::PANEL_ENTER)
            .with_easing(|t| easing::cubic_bezier(easing::ENTER, t));
        let motion_id: SharedString = format!("{}-open-{}", self.id, self.opening).into();
        let animated = move |dock: Div| {
            dock.with_animation(
                ElementId::Name(motion_id.clone()),
                enter.clone(),
                move |dock, delta| {
                    let dock = dock.opacity(delta);
                    let shift = px(travel * (1. - delta));
                    match (reduce, mode) {
                        (true, _) => dock,
                        // In flow: a relative offset.
                        (false, DockMode::Split { .. }) => dock.left(shift),
                        // Pinned to the right edge: move that edge.
                        (false, DockMode::Overlay { .. }) => dock.right(-shift),
                        (false, DockMode::Full) => dock.left(shift).right(-shift),
                    }
                },
            )
        };
        let column = div()
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .min_w_0()
            .bg(c.surface_panel)
            .child(dock);
        let region = div().relative().flex().flex_1().min_w_0().h_full();
        match mode {
            DockMode::Split { width } => {
                let current = self.split.current(mode, self.available);
                let (on_resize, on_commit) = (self.on_resize.clone(), self.on_commit.clone());
                let handle = ResizeHandle::new(
                    SharedString::from(format!("{}-resize", self.id)),
                    current,
                    self.split.min,
                    self.split.max(self.available),
                )
                .leading()
                .on_resize(move |width, window, cx| on_resize(width, window, cx))
                .on_commit(move |window, cx| on_commit(window, cx));
                let (page, column) = match width {
                    // The board's 1.1fr / 1fr, with its 320 and 316 minimums.
                    None => (
                        div()
                            .flex()
                            .flex_grow(1.1)
                            .flex_basis(px(0.))
                            .min_w(px(320.))
                            .h_full()
                            .child(page),
                        column
                            .flex_grow(1.)
                            .flex_basis(px(0.))
                            .min_w(px(self.split.min)),
                    ),
                    Some(width) => (
                        div().flex().flex_1().min_w_0().h_full().child(page),
                        column.flex_none().w(px(width)),
                    ),
                };
                region
                    .child(page)
                    .child(
                        animated(column.border_l_1().border_color(c.border_standard))
                            .into_any_element(),
                    )
                    .child(
                        // On the dock's left edge, over the divider.
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right(px(current))
                            .w_0()
                            .child(handle),
                    )
                    .into_any_element()
            }
            DockMode::Overlay { width } => region
                .child(div().flex().flex_1().min_w_0().h_full().child(page))
                .child(
                    animated(
                        column
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(px(width))
                            .border_l_1()
                            .border_color(c.border_standard)
                            .shadow(shadow::sm()),
                    )
                    .into_any_element(),
                )
                .into_any_element(),
            DockMode::Full => region
                .child(div().flex().flex_1().min_w_0().h_full().child(page))
                .child(
                    animated(column.absolute().top_0().bottom_0().left_0().right_0())
                        .into_any_element(),
                )
                .into_any_element(),
        }
    }
}

/// A 40px panel header close button, as buzz-app's toolbar IconButton.
fn close_button(id: impl Into<ElementId>, label: SharedString, close: Close) -> impl IntoElement {
    IconButton::new(id, IconName::X)
        .size(IconButtonSize::Sm)
        .icon_size(18.)
        .tooltip(label)
        .on_click(move |_, window, cx| close(window, cx))
}

/// A panel outside a tab workspace (`PanelCard`): a header with its title
/// and a close button, Escape closing it.
pub fn card(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    icon: Option<IconName>,
    content: AnyElement,
    close: Close,
    cx: &App,
) -> AnyElement {
    let c = cx.colors();
    let title: SharedString = title.into();
    let id: SharedString = id.into();
    let escape = close.clone();
    div()
        .id(id.clone())
        .flex()
        .flex_col()
        .size_full()
        .min_w_0()
        .text_body_sm()
        .text_color(c.text_standard)
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape(window, cx);
            }
        })
        .child(
            PanelHeader::new()
                .joined(true)
                .label(icon, title.clone())
                .action(close_button(
                    SharedString::from(format!("{id}-close")),
                    format!("Close {title} panel").into(),
                    close,
                )),
        )
        .child(
            div()
                .id(SharedString::from(format!("{id}-content")))
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(content),
        )
        .into_any_element()
}

/// A drill-in view over a panel's base card (`PanelSubview`): a back
/// arrow, the title, and a close button for the whole panel. The base
/// stays laid out underneath, so it keeps its scroll position. In
/// buzz-app only an agent profile's harness log drills in, which the
/// profiles slice has yet to port.
#[allow(dead_code)]
pub struct Subview {
    pub title: SharedString,
    pub back_label: SharedString,
    pub on_back: Close,
    pub on_close: Option<Close>,
    pub content: AnyElement,
}

#[allow(dead_code)]
pub fn with_subview(base: AnyElement, subview: Option<Subview>, cx: &App) -> AnyElement {
    let Some(subview) = subview else {
        return base;
    };
    let c = cx.colors();
    let back = subview.on_back.clone();
    let escape = subview.on_back.clone();
    let title = subview.title.clone();
    let mut header = PanelHeader::new().joined(true).leading(
        IconButton::new("subview-back", IconName::ArrowLeft)
            .size(IconButtonSize::Sm)
            .icon_size(18.)
            .tooltip(subview.back_label)
            .on_click(move |_, window, cx| back(window, cx)),
    );
    header = header.leading(div().text_label().child(title.clone()));
    if let Some(close) = subview.on_close {
        header = header.action(close_button(
            "subview-close",
            format!("Close {} panel", title.to_lowercase()).into(),
            close,
        ));
    }
    div()
        .relative()
        .size_full()
        .child(div().size_full().invisible().child(base))
        .child(
            div()
                .id("panel-subview")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .flex_col()
                .bg(c.surface_panel)
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        escape(window, cx);
                    }
                })
                .child(header)
                .child(
                    div()
                        .id("panel-subview-content")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(subview.content),
                ),
        )
        .into_any_element()
}

/// Scrolls the tab strip so a tab laid out at `tab` shows, as buzz-app
/// does when a tab is selected. False until both have been laid out.
pub fn reveal_tab(strip: &ScrollHandle, tab: Option<Bounds<Pixels>>) -> bool {
    let view = strip.bounds();
    let Some(tab) = tab.filter(|_| view.size.width > px(0.)) else {
        return false;
    };
    let offset = strip.offset();
    let shift = if tab.left() < view.left() {
        view.left() - tab.left()
    } else if tab.right() > view.right() {
        view.right() - tab.right()
    } else {
        return true;
    };
    strip.set_offset(point((offset.x + shift).min(px(0.)), offset.y));
    true
}

/// Shows the tab with this id.
pub type SelectTab = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;

/// One tab of a [`tabs`] workspace.
pub struct TabItem {
    pub id: SharedString,
    pub label: SharedString,
    pub icon: IconName,
    pub content: AnyElement,
    pub close: Close,
    /// A thread tab the pointer can drag.
    pub drag: Option<TabDrag>,
}

/// The channel's tabbed panel workspace (`PanelWorkspace`): navigation
/// tabs with a close button each, then the selected tab's content. Tabs
/// open from content; there is no blank tab to add. Escape closes the
/// selected tab. With `geometry`, its tabs and header record where they
/// are, and its thread tabs can be dragged.
pub fn tabs(
    items: Vec<TabItem>,
    selected: &str,
    strip_scroll: &ScrollHandle,
    on_select: SelectTab,
    geometry: Option<&Geometry>,
    cx: &App,
) -> AnyElement {
    let c = cx.colors();
    let mut strip = Tabs::new(TabVariant::Navigation);
    let mut content = None;
    let mut active_close = None;
    for item in items {
        let is_selected = item.id.as_ref() == selected;
        let select = on_select.clone();
        let id = item.id.clone();
        let close = item.close.clone();
        let mut tab = Tab::new(SharedString::from(format!("tab-{}", item.id)), item.label)
            .icon(item.icon)
            .selected(is_selected)
            .on_click(move |_, window, cx| select(id.clone(), window, cx))
            .on_close(move |_, window, cx| close(window, cx));
        if let Some(geometry) = geometry {
            tab = tab
                .draggable(item.drag.is_some())
                .extend(tab_drag::tab_handlers(
                    geometry,
                    item.id.to_string(),
                    item.drag,
                ));
        }
        strip = strip.tab(tab);
        if is_selected {
            content = Some(item.content);
            active_close = Some(item.close);
        }
    }
    // The strip scrolls sideways.
    let header = div()
        .flex()
        .flex_col()
        .flex_none()
        .child(
            div()
                .id("panel-tab-header")
                .relative()
                .when_some(geometry, |header, geometry| {
                    header.child(tab_drag::record(geometry, |geometry, bounds| {
                        geometry.headers.insert(PaneKey::Side, bounds);
                    }))
                })
                .flex()
                .items_center()
                .gap(rems(0.5))
                .min_h(rems(2.5))
                .px(rems(0.25))
                .child(
                    div()
                        .id("panel-tab-strip")
                        .flex_1()
                        .min_w_0()
                        .overflow_x_scroll()
                        .track_scroll(strip_scroll)
                        .child(strip),
                ),
        )
        .child(div().h(px(1.)).bg(c.border_standard));
    div()
        .id("panel-workspace")
        .flex()
        .flex_col()
        .size_full()
        .min_w_0()
        .text_body_sm()
        .text_color(c.text_standard)
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape"
                && let Some(close) = &active_close
            {
                cx.stop_propagation();
                close(window, cx);
            }
        })
        .child(header)
        .child(div().flex().flex_col().flex_1().min_h_0().children(content))
        .into_any_element()
}
