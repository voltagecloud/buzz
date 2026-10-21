//! Dragging thread tabs (`useTabDrag.tsx`, `TabDragFeedback.tsx`): Panel
//! Swap's title-only gesture. The tab's title follows the pointer, a line
//! marks where it would land in a pane's tab strip, and dragging a main
//! tab to the main pane's right edge, while the side pane is closed,
//! marks a separation into it. Only the conversation's owner commits the
//! move; Escape or a resized window cancels it.

use crate::ui::channel_tabs::TabPane;
use buzz_ui::{ActiveTheme, TypeStyled};
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The tab being dragged.
#[derive(Clone)]
pub struct TabDrag {
    pub id: String,
    pub label: SharedString,
    pub pane: TabPane,
}

/// How the current gesture started.
#[derive(Clone, Copy, Debug)]
pub struct DragStart {
    /// Where the pointer was, for the edge separation's travel.
    pub at: Point<Pixels>,
    /// The window's size; a resize cancels the gesture.
    pub viewport: Size<Pixels>,
}

/// Where tabs and pane headers were laid out this frame, for hit testing.
#[derive(Default)]
pub struct TabGeometry {
    pub tabs: HashMap<String, Bounds<Pixels>>,
    pub headers: HashMap<PaneKey, Bounds<Pixels>>,
    /// The whole main pane, header included.
    pub main: Option<Bounds<Pixels>>,
    pub start: Option<DragStart>,
}

/// A pane, as a map key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneKey {
    Main,
    Side,
}

impl From<TabPane> for PaneKey {
    fn from(pane: TabPane) -> Self {
        match pane {
            TabPane::Main => Self::Main,
            TabPane::Side => Self::Side,
        }
    }
}

pub type Geometry = Rc<RefCell<TabGeometry>>;

/// The main pane's own conversation tab, as its geometry is keyed.
pub const CONVERSATION: &str = "conversation";

impl TabGeometry {
    /// Forgets the last frame's layout, which is returned: what is drawn
    /// again records itself.
    pub fn clear(geometry: &Geometry) -> HashMap<String, Bounds<Pixels>> {
        let mut geometry = geometry.borrow_mut();
        geometry.headers.clear();
        geometry.main = None;
        std::mem::take(&mut geometry.tabs)
    }

    /// Ends the gesture.
    pub fn finish(geometry: &Geometry) {
        geometry.borrow_mut().start = None;
    }
}

/// Records an element's bounds into `geometry` as it is laid out.
pub fn record(
    geometry: &Geometry,
    store: impl Fn(&mut TabGeometry, Bounds<Pixels>) + 'static,
) -> impl IntoElement {
    let geometry = geometry.clone();
    canvas(
        move |bounds, _, _| store(&mut geometry.borrow_mut(), bounds),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Where a drop would land.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropTarget {
    pub pane: TabPane,
    /// The position among the pane's movable tabs.
    pub index: usize,
    /// A separation into the closed side pane.
    pub edge: bool,
    /// The insertion line's top, or the edge mark's center.
    pub at: Point<Pixels>,
    pub height: Pixels,
}

/// The tabs one pane orders, left to right.
pub struct PaneOrder<'a> {
    pub pane: TabPane,
    /// The tabs a drop is placed among.
    pub tabs: Vec<&'a str>,
    /// Every tab the strip shows, fixed ones too, in order.
    pub strip: Vec<&'a str>,
}

/// What a drag may do this frame.
pub struct Rules<'a> {
    pub order: Vec<PaneOrder<'a>>,
    /// The panes the tab may move to.
    pub can_move: &'a dyn Fn(TabPane) -> bool,
    /// The side pane shows.
    pub side_visible: bool,
    /// Where an edge separation appends in the closed side pane.
    pub closed_side_index: Option<usize>,
}

/// The drop target under `position` (`target` in `useTabDrag`), for a
/// drag that started at `start`.
pub fn target(
    geometry: &TabGeometry,
    drag: &TabDrag,
    start: Point<Pixels>,
    position: Point<Pixels>,
    rules: &Rules,
    previous: Option<DropTarget>,
) -> Option<DropTarget> {
    for order in &rules.order {
        if !(rules.can_move)(order.pane) {
            continue;
        }
        let Some(header) = geometry.headers.get(&order.pane.into()) else {
            continue;
        };
        let tolerance = match previous {
            Some(previous) if previous.pane == order.pane && !previous.edge => px(12.),
            _ => px(0.),
        };
        if position.x < header.left() - tolerance
            || position.x > header.right() + tolerance
            || position.y < header.top() - tolerance
            || position.y > header.bottom() + tolerance.max(px(8.))
        {
            continue;
        }
        let tabs: Vec<Bounds<Pixels>> = order
            .tabs
            .iter()
            .filter(|id| **id != drag.id)
            .filter_map(|id| geometry.tabs.get(*id).copied())
            .collect();
        let before = tabs
            .iter()
            .position(|tab| position.x < tab.left() + tab.size.width / 2.);
        let index = before.unwrap_or(tabs.len());
        let last = order
            .strip
            .iter()
            .rev()
            .filter(|id| **id != drag.id)
            .find_map(|id| geometry.tabs.get(*id).copied());
        let anchor = before
            .map(|index| tabs[index])
            .or(tabs.last().copied())
            .or(last);
        let rect = anchor.unwrap_or(*header);
        let boundary = if before.is_some() {
            rect.left()
        } else {
            rect.right()
        };
        return Some(DropTarget {
            pane: order.pane,
            index,
            edge: false,
            at: point(boundary.clamp(header.left(), header.right()), rect.top()),
            height: rect.size.height,
        });
    }
    let main = geometry.main?;
    let index = rules.closed_side_index?;
    if drag.pane != TabPane::Main || rules.side_visible || !(rules.can_move)(TabPane::Side) {
        return None;
    }
    let header_bottom = geometry
        .headers
        .get(&PaneKey::Main)
        .map_or(main.top(), |header| header.bottom());
    // Header insertion wins; below it, a deliberate move to the resting
    // pane's right edge separates the tab into the side pane.
    let band = (main.size.width * 0.22).clamp(px(48.), px(120.));
    let tolerance = match previous {
        Some(previous) if previous.edge => px(16.),
        _ => px(0.),
    };
    let edge = position.x - start.x > px(24.)
        && position.x >= main.right() - band - tolerance
        && position.x <= main.right() + px(48.)
        && position.y > header_bottom + px(12.)
        && position.y <= main.bottom();
    edge.then(|| DropTarget {
        pane: TabPane::Side,
        index,
        edge: true,
        at: point(main.right(), main.top() + main.size.height / 2.),
        height: px(0.),
    })
}

/// Scrolls a strip held near either end (24px) by 8px toward it.
pub fn scroll_near_ends(strip: &ScrollHandle, position: Point<Pixels>) {
    let bounds = strip.bounds();
    let step = if position.x < bounds.left() + px(24.) {
        px(8.)
    } else if position.x > bounds.right() - px(24.) {
        px(-8.)
    } else {
        return;
    };
    let offset = strip.offset();
    let widest = strip.max_offset().x.max(px(0.));
    let x = (offset.x + step).clamp(-widest, px(0.));
    strip.set_offset(point(x, offset.y));
}

/// The title that follows the pointer, the size of the tab it left.
pub struct DragTitle {
    label: SharedString,
    size: Size<Pixels>,
}

/// Starts a tab's drag: keeps where it started, and makes the title.
pub fn start(
    geometry: &Geometry,
    drag: &TabDrag,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DragTitle> {
    let mut geometry = geometry.borrow_mut();
    geometry.start = Some(DragStart {
        at: window.mouse_position(),
        viewport: window.viewport_size(),
    });
    let size = geometry
        .tabs
        .get(&drag.id)
        .map(|bounds| bounds.size)
        .unwrap_or(size(px(192.), px(32.)));
    let label = drag.label.clone();
    cx.new(|_| DragTitle { label, size })
}

/// A tab's part in drags: it records where it is, so drops are placed
/// around it, and a movable tab's drag carries its title.
pub fn tab_handlers(
    geometry: &Geometry,
    id: String,
    drag: Option<TabDrag>,
) -> impl FnOnce(Stateful<Div>) -> Stateful<Div> + 'static {
    let (record_geometry, start_geometry) = (geometry.clone(), geometry.clone());
    move |tab| {
        let tab = tab.child(record(&record_geometry, move |geometry, bounds| {
            geometry.tabs.insert(id.clone(), bounds);
        }));
        match drag {
            Some(drag) => tab.on_drag(drag, move |drag, _, window, cx| {
                start(&start_geometry, drag, window, cx)
            }),
            None => tab,
        }
    }
}

impl Render for DragTitle {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.colors();
        div()
            .w(self.size.width)
            .h(self.size.height)
            .flex()
            .items_center()
            .px(rems(0.75))
            .py(rems(0.25))
            .rounded_full()
            .bg(c.affordance_accent)
            .text_color(c.text_standard)
            .opacity(0.85)
            .text_body()
            .cursor(CursorStyle::ClosedHand)
            .child(div().min_w_0().truncate().child(self.label.clone()))
    }
}

/// The insertion line, or the edge mark, where the drop would land.
pub fn feedback(target: Option<DropTarget>, cx: &App) -> Option<AnyElement> {
    let target = target?;
    let c = cx.colors();
    let mark = if target.edge {
        // A short pill against the main pane's right edge.
        div()
            .w(px(2.))
            .h(px(24.))
            .rounded_full()
            .bg(c.affordance_prominent)
            .into_any_element()
    } else {
        div()
            .w(px(3.))
            .h((target.height - px(16.)).max(px(0.)))
            .rounded_full()
            .bg(c.border_accent)
            .into_any_element()
    };
    let at = if target.edge {
        point(target.at.x - px(2.), target.at.y - px(12.))
    } else {
        point(target.at.x - px(1.5), target.at.y + px(8.))
    };
    Some(
        deferred(anchored().position(at).child(mark))
            .with_priority(3)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn drag(id: &str, pane: TabPane) -> TabDrag {
        TabDrag {
            id: id.into(),
            label: "Thread".into(),
            pane,
        }
    }

    const START: Point<Pixels> = Point {
        x: px(100.),
        y: px(20.),
    };

    fn geometry() -> TabGeometry {
        let mut geometry = TabGeometry::default();
        geometry
            .headers
            .insert(PaneKey::Main, bounds(0., 0., 600., 40.));
        geometry.main = Some(bounds(0., 0., 600., 800.));
        geometry
            .tabs
            .insert("a".into(), bounds(150., 4., 100., 32.));
        geometry
            .tabs
            .insert("b".into(), bounds(260., 4., 100., 32.));
        geometry
    }

    #[test]
    fn a_header_drop_lands_before_the_tab_whose_middle_is_right_of_it() {
        let geometry = geometry();
        let order = vec![PaneOrder {
            pane: TabPane::Main,
            tabs: vec!["a", "b"],
            strip: vec!["a", "b"],
        }];
        let rules = Rules {
            order,
            can_move: &|_| true,
            side_visible: false,
            closed_side_index: None,
        };
        let dragged = drag("c", TabPane::Side);
        let at = |x: f32| {
            target(
                &geometry,
                &dragged,
                START,
                point(px(x), px(20.)),
                &rules,
                None,
            )
        };
        assert_eq!(at(160.).map(|t| t.index), Some(0));
        assert_eq!(at(210.).map(|t| t.index), Some(1));
        assert_eq!(at(400.).map(|t| t.index), Some(2));
        assert_eq!(at(400.).unwrap().at.x, px(360.));
        // Below the header, without an edge to separate into, nothing.
        assert_eq!(
            target(
                &geometry,
                &dragged,
                START,
                point(px(200.), px(300.)),
                &rules,
                None
            ),
            None
        );
    }

    #[test]
    fn a_main_tab_separates_into_the_closed_side_pane_at_the_right_edge() {
        let geometry = geometry();
        let rules = Rules {
            order: Vec::new(),
            can_move: &|_| true,
            side_visible: false,
            closed_side_index: Some(2),
        };
        let dragged = drag("a", TabPane::Main);
        let edge = target(
            &geometry,
            &dragged,
            START,
            point(px(590.), px(300.)),
            &rules,
            None,
        )
        .unwrap();
        assert!(edge.edge);
        assert_eq!((edge.pane, edge.index), (TabPane::Side, 2));
        // Short of the band, or while the side pane shows, it does not.
        assert_eq!(
            target(
                &geometry,
                &dragged,
                START,
                point(px(300.), px(300.)),
                &rules,
                None
            ),
            None
        );
        let open = Rules {
            side_visible: true,
            ..rules
        };
        assert_eq!(
            target(
                &geometry,
                &dragged,
                START,
                point(px(590.), px(300.)),
                &open,
                None
            ),
            None
        );
    }
}
