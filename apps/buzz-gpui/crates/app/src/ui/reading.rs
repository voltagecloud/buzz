//! What a message list shows, for automatic reading (`use-reading.ts`):
//! only rows wholly in view count, only while the window is active and
//! focus is in the list or its composer, and only reports that changed
//! are sent, so a redraw does not restart the session's dwell.

use std::cell::RefCell;
use std::rc::Rc;

use buzz_relay_client::EventId;
use gpui_kit::*;

/// Most rows one report names.
const MOST: usize = 128;

/// What a list shows after its last layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// Rows wholly inside the viewport, top first.
    pub visible: Vec<EventId>,
    /// Rows at least partly inside it.
    pub shown: Vec<EventId>,
    /// The list sits at its end.
    pub at_bottom: bool,
}

/// Where a list's rows were laid out in the frame being drawn. GPUI's
/// `ListState::bounds_for_item` knows no row while a bottom-aligned list
/// rests at its end, as a channel timeline usually does: its logical
/// scroll top is then past the last row. So each row records its own
/// bounds as it is prepainted, and the reader takes them afterwards.
#[derive(Clone, Default)]
pub struct Drawn(Rc<RefCell<Vec<Row>>>);

/// A list index and where that row was drawn.
type Row = (usize, Bounds<Pixels>);

impl Drawn {
    /// An empty layer over row `ix` that records where it is drawn.
    pub fn probe(&self, ix: usize) -> impl IntoElement {
        let rows = self.clone();
        canvas(
            move |bounds, _, _| rows.0.borrow_mut().push((ix, bounds)),
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    /// The rows drawn since the last call, top first.
    fn take(&self) -> Vec<Row> {
        let mut rows = std::mem::take(&mut *self.0.borrow_mut());
        rows.sort_by_key(|(ix, _)| *ix);
        rows.dedup_by_key(|(ix, _)| *ix);
        rows
    }
}

/// Reads `list` after layout, from the rows it drew. `id` names the
/// message at a list index, or `None` for rows that are not read: edges,
/// notes and membership rows.
pub fn view(list: &ListState, drawn: &Drawn, id: impl Fn(usize) -> Option<EventId>) -> View {
    visible(list.viewport_bounds(), at_bottom(list), drawn.take(), id)
}

fn visible(
    viewport: Bounds<Pixels>,
    at_bottom: bool,
    rows: Vec<Row>,
    id: impl Fn(usize) -> Option<EventId>,
) -> View {
    let mut view = View {
        at_bottom,
        ..View::default()
    };
    if viewport.size.height <= px(0.) {
        return view;
    }
    for (ix, bounds) in rows {
        let Some(id) = id(ix) else {
            continue;
        };
        if bounds.bottom() <= viewport.top()
            || bounds.top() >= viewport.bottom()
            || bounds.size.height <= px(0.)
        {
            continue;
        }
        view.shown.push(id);
        if bounds.top() >= viewport.top()
            && bounds.bottom() <= viewport.bottom()
            && view.visible.len() < MOST
        {
            view.visible.push(id);
        }
    }
    view
}

/// Within a pixel of the end, as the reference's catch-up requires.
pub fn at_bottom(list: &ListState) -> bool {
    if list.is_following_tail() {
        return true;
    }
    let max = list.max_offset_for_scrollbar().y;
    let offset = -list.scroll_px_offset_for_scrollbar().y;
    max - offset <= px(1.)
}

/// How far the list is from its end.
pub fn distance_from_bottom(list: &ListState) -> Pixels {
    if list.is_following_tail() {
        return px(0.);
    }
    let max = list.max_offset_for_scrollbar().y;
    let offset = -list.scroll_px_offset_for_scrollbar().y;
    (max - offset).max(px(0.))
}

/// The last report sent, so only changes go out.
#[derive(Default)]
pub struct Reporter {
    last: Option<(Vec<EventId>, bool)>,
}

impl Reporter {
    /// What to report now, if it differs from the last report. An inactive
    /// surface reports nothing in view and not at the bottom.
    pub fn next(&mut self, view: &View, active: bool) -> Option<(Vec<EventId>, bool)> {
        let report = if active {
            (view.visible.clone(), view.at_bottom)
        } else {
            (Vec::new(), false)
        };
        if self.last.as_ref() == Some(&report) {
            return None;
        }
        self.last = Some(report.clone());
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn reports_only_changes_and_nothing_while_inactive() {
        let id = EventId::from_byte_array([1; 32]);
        let view = View {
            visible: vec![id],
            shown: vec![id],
            at_bottom: true,
        };
        let mut reporter = Reporter::default();
        assert_eq!(reporter.next(&view, true), Some((vec![id], true)));
        assert_eq!(reporter.next(&view, true), None);
        assert_eq!(reporter.next(&view, false), Some((Vec::new(), false)));
        assert_eq!(reporter.next(&view, false), None);
        assert_eq!(reporter.next(&view, true), Some((vec![id], true)));
    }

    /// A bottom-aligned list resting at its end has no item bounds in
    /// `ListState`, so the rows it drew are what tell the reader.
    #[test]
    fn rows_drawn_at_the_bottom_are_read() {
        let row =
            |top: f32, height: f32| Bounds::new(point(px(0.), px(top)), size(px(100.), px(height)));
        let ids: Vec<EventId> = (0..5).map(|n| EventId::from_byte_array([n; 32])).collect();
        let rows = Drawn::default();
        // Drawn out of order, one twice; row 0 is the history's edge.
        for (ix, bounds) in [
            (3, row(140., 40.)),
            (0, row(0., 60.)),
            (1, row(60., 60.)),
            (2, row(120., 20.)),
            (3, row(140., 40.)),
            (4, row(180., 40.)),
        ] {
            rows.0.borrow_mut().push((ix, bounds));
        }
        let viewport = Bounds::new(point(px(0.), px(80.)), size(px(100.), px(120.)));
        let id = |ix: usize| (ix > 0).then(|| ids[ix]);
        let view = visible(viewport, true, rows.take(), id);
        assert_eq!(view.visible, vec![ids[2], ids[3]]);
        assert_eq!(view.shown, vec![ids[1], ids[2], ids[3], ids[4]]);
        assert!(view.at_bottom);
        // Each frame reads only what it drew.
        assert!(rows.take().is_empty());
    }
}
