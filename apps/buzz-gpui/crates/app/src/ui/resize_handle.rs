//! The divider grip between the sidebar or dock and the page
//! (`features/panels/ResizeHandle.tsx`, ui.md §2.8).
//!
//! gpui-kit's resizable group owns its panels' sizes and draws a different
//! pill, so this hand-rolls buzz-app's handle: a 16px hit area centred on
//! the divider, a 3x56 grip on hover, focus or drag, drag and arrow-key
//! resizing, Home/End, and a double-click reset.

use super::workspace::layout::{LARGE_STEP, STEP};
use buzz_ui::{ActiveTheme, Tooltip};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;

/// The new width, or `None` to reset to the default.
pub type OnResize = Rc<dyn Fn(Option<f32>, &mut Window, &mut App)>;
/// A drag or key press finished changing the width, so it can be saved.
pub type OnCommit = Rc<dyn Fn(&mut Window, &mut App)>;

const HIT: f32 = 16.;
const TOOLTIP: &str = "Drag to resize · Double-click to reset";

#[derive(IntoElement)]
pub struct ResizeHandle {
    id: SharedString,
    width: f32,
    min: f32,
    max: f32,
    /// 1 when dragging right widens (the sidebar), -1 when dragging left
    /// does (a dock on the right).
    direction: f32,
    on_resize: OnResize,
    on_commit: Option<OnCommit>,
    /// The grip's opacity while shown (`--resize-grip-opacity`).
    grip: f32,
}

/// What travels with a drag, so each handle answers only its own.
struct ResizeDrag(SharedString);

struct DragView;

impl Render for DragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl ResizeHandle {
    pub fn new(id: impl Into<SharedString>, width: f32, min: f32, max: f32) -> Self {
        Self {
            id: id.into(),
            width,
            min,
            max,
            direction: 1.,
            on_resize: Rc::new(|_, _, _| {}),
            on_commit: None,
            grip: 1.,
        }
    }

    /// A quieter grip, for a handle in a gap between surfaces.
    pub fn grip_opacity(mut self, opacity: f32) -> Self {
        self.grip = opacity;
        self
    }

    /// The handle sits on the left edge of what it resizes.
    pub fn leading(mut self) -> Self {
        self.direction = -1.;
        self
    }

    pub fn on_resize(mut self, f: impl Fn(Option<f32>, &mut Window, &mut App) + 'static) -> Self {
        self.on_resize = Rc::new(f);
        self
    }

    pub fn on_commit(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_commit = Some(Rc::new(f));
        self
    }
}

/// The width a key press asks for, if the key resizes.
fn keyed(key: &str, shift: bool, direction: f32, width: f32, min: f32, max: f32) -> Option<f32> {
    let step = if shift { LARGE_STEP } else { STEP };
    let next = match key {
        "right" => width + step * direction,
        "left" => width - step * direction,
        "home" => min,
        "end" => max,
        _ => return None,
    };
    Some(next.clamp(min, max))
}

impl RenderOnce for ResizeHandle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = *cx.colors();
        let id = self.id.clone();
        let focus = window
            .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
                cx.focus_handle()
            })
            .read(cx)
            .clone();
        // The pointer's x and the width when the drag began.
        let start = window
            .use_keyed_state(SharedString::from(format!("{id}-start")), cx, |_, _| {
                Rc::new(Cell::new(None::<(Pixels, f32)>))
            })
            .read(cx)
            .clone();
        let dragging = start.get().is_some() && cx.has_active_drag();
        let engaged = dragging || focus.is_focused(window);
        let (width, min, max, direction) = (self.width, self.min, self.max, self.direction);
        let grip = self.grip;
        let group = SharedString::from(format!("{id}-group"));

        let on_resize = self.on_resize.clone();
        let on_resize_key = self.on_resize.clone();
        let on_resize_reset = self.on_resize.clone();
        let commit_key = self.on_commit.clone();
        let commit_up = self.on_commit.clone();
        let commit_out = self.on_commit.clone();
        let (start_down, start_move, start_up, start_out) =
            (start.clone(), start.clone(), start.clone(), start);
        let drag_id = id.clone();
        let finish = move |start: &Rc<Cell<Option<(Pixels, f32)>>>,
                           commit: &Option<OnCommit>,
                           window: &mut Window,
                           cx: &mut App| {
            if start.take().is_some()
                && let Some(commit) = commit
            {
                commit(window, cx);
            }
        };
        let finish_out = finish;

        div()
            .id(id.clone())
            .group(group.clone())
            .track_focus(&focus)
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(-HIT / 2.))
            .w(px(HIT))
            .occlude()
            .cursor(CursorStyle::ResizeLeftRight)
            .when(!dragging, |this| this.tooltip(Tooltip::text(TOOLTIP)))
            .on_mouse_down(MouseButton::Left, move |event, _, _| {
                start_down.set(Some((event.position.x, width)));
            })
            .on_drag(ResizeDrag(id.clone()), |_, _, _, cx| cx.new(|_| DragView))
            .on_drag_move::<ResizeDrag>(move |event, window, cx| {
                if event.drag(cx).0 != drag_id {
                    return;
                }
                let Some((x, from)) = start_move.get() else {
                    return;
                };
                let moved = f32::from(event.event.position.x - x) * direction;
                on_resize(Some((from + moved).round().clamp(min, max)), window, cx);
            })
            .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                finish(&start_up, &commit_up, window, cx)
            })
            .on_mouse_up_out(MouseButton::Left, move |_, window, cx| {
                finish_out(&start_out, &commit_out, window, cx)
            })
            .on_click(move |event, window, cx| {
                if event.click_count() == 2 {
                    on_resize_reset(None, window, cx);
                }
            })
            .on_key_down(move |event, window, cx| {
                let keys = &event.keystroke;
                if let Some(next) =
                    keyed(&keys.key, keys.modifiers.shift, direction, width, min, max)
                {
                    cx.stop_propagation();
                    on_resize_key(Some(next), window, cx);
                    if let Some(commit) = &commit_key {
                        commit(window, cx);
                    }
                }
            })
            .child(
                // Centred on the divider's last pixel (`50% - 0.5px`).
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(HIT / 2. - 2.))
                    .w(px(3.))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(3.))
                            .h(px(56.))
                            .rounded_full()
                            .bg(c.border_focus)
                            .opacity(if engaged { grip } else { 0. })
                            .group_hover(group, move |this| this.opacity(grip)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn keys_step_and_jump_within_range() {
        assert_eq!(keyed("right", false, 1., 260., 220., 520.), Some(276.));
        assert_eq!(keyed("right", true, 1., 260., 220., 520.), Some(308.));
        // A dock on the right grows as its left edge moves left.
        assert_eq!(keyed("left", false, -1., 420., 300., 700.), Some(436.));
        assert_eq!(keyed("left", true, 1., 230., 220., 520.), Some(220.));
        assert_eq!(keyed("home", false, 1., 300., 220., 520.), Some(220.));
        assert_eq!(keyed("end", false, 1., 300., 220., 520.), Some(520.));
        assert_eq!(keyed("up", false, 1., 300., 220., 520.), None);
    }
}
