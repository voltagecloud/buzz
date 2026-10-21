//! Click-and-drag scrubbing, the `input type="range"` of the reference
//! players, with the mouse-only hover position the time previews follow.

use gpui_kit::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Called with the pointer's position along the bar, 0..=1.
pub type OnSeek = Rc<dyn Fn(f32, &mut Window, &mut App)>;
/// Called with the hovered position along the bar on every mouse move,
/// `None` while the pointer is elsewhere; the owner ignores repeats.
pub type OnHover = Rc<dyn Fn(Option<f32>, &mut Window, &mut App)>;

thread_local! {
    /// The bar a press started on, so a drag keeps scrubbing it even when
    /// the pointer strays off its row.
    static DRAGGING: RefCell<Option<ElementId>> = const { RefCell::new(None) };
}

/// Which way a bar runs. Vertical bars fill from the bottom.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Covers its `relative` parent and reports presses and drags on it.
pub fn seek_area(id: impl Into<ElementId>, on_seek: Option<OnSeek>) -> impl IntoElement {
    bar(id.into(), Axis::Horizontal, on_seek, None)
}

/// [`seek_area`] that also reports where the mouse hovers.
pub fn seek_area_hover(
    id: impl Into<ElementId>,
    on_seek: Option<OnSeek>,
    on_hover: OnHover,
) -> impl IntoElement {
    bar(id.into(), Axis::Horizontal, on_seek, Some(on_hover))
}

/// A vertical [`seek_area`], 0 at the bottom.
pub fn seek_area_vertical(id: impl Into<ElementId>, on_seek: OnSeek) -> impl IntoElement {
    bar(id.into(), Axis::Vertical, Some(on_seek), None)
}

/// Where `point` falls along `bounds`, 0..=1. Horizontal bars leave a
/// pixel at each end, as the reference's thumb does
/// (`(x - left - 1) / (width - 2)`).
pub fn fraction(axis: Axis, bounds: Bounds<Pixels>, point: Point<Pixels>) -> f32 {
    match axis {
        Axis::Horizontal => {
            let span = (f32::from(bounds.size.width) - 2.).max(1.);
            ((f32::from(point.x - bounds.left()) - 1.) / span).clamp(0., 1.)
        }
        Axis::Vertical => {
            let span = f32::from(bounds.size.height).max(1.);
            (f32::from(bounds.bottom() - point.y) / span).clamp(0., 1.)
        }
    }
}

fn bar(
    id: ElementId,
    axis: Axis,
    on_seek: Option<OnSeek>,
    on_hover: Option<OnHover>,
) -> impl IntoElement {
    canvas(
        move |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, _| {
            if let Some(on_hover) = on_hover {
                let hover_hitbox = hitbox.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let over = hover_hitbox.is_hovered(window);
                    on_hover(
                        over.then(|| fraction(axis, bounds, event.position)),
                        window,
                        cx,
                    );
                });
            }
            let Some(on_seek) = on_seek else {
                return;
            };
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            let (press_id, press_seek) = (id.clone(), on_seek.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                DRAGGING.with_borrow_mut(|dragging| *dragging = Some(press_id.clone()));
                press_seek(fraction(axis, bounds, event.position), window, cx);
                cx.stop_propagation();
            });
            let drag_id = id.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                let ours = DRAGGING.with_borrow(|dragging| dragging.as_ref() == Some(&drag_id));
                if phase == DispatchPhase::Bubble && ours && event.dragging() {
                    on_seek(fraction(axis, bounds, event.position), window, cx);
                }
            });
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, _| {
                if phase == DispatchPhase::Bubble {
                    DRAGGING.with_borrow_mut(|dragging| {
                        if dragging.as_ref() == Some(&id) {
                            *dragging = None;
                        }
                    });
                }
            });
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}

/// A range's keyboard step (`input type="range"`): arrows by `step`
/// (up and right raise it), Page Up and Down by a tenth, Home and End to
/// the ends. `None` for other keys.
pub fn key_step(key: &str, value: f64, max: f64, step: f64) -> Option<f64> {
    let page = (max / 10.).max(step);
    let next = match key {
        "right" | "up" => value + step,
        "left" | "down" => value - step,
        "pageup" => value + page,
        "pagedown" => value - page,
        "home" => 0.,
        "end" => max,
        _ => return None,
    };
    Some(next.clamp(0., max.max(0.)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn measures_bars_like_a_range_input() {
        let bounds = Bounds::new(point(px(10.), px(0.)), size(px(102.), px(40.)));
        let at = |x: f32, y: f32| point(px(x), px(y));
        assert_eq!(fraction(Axis::Horizontal, bounds, at(11., 0.)), 0.);
        assert_eq!(fraction(Axis::Horizontal, bounds, at(61., 0.)), 0.5);
        assert_eq!(fraction(Axis::Horizontal, bounds, at(500., 0.)), 1.);
        assert_eq!(fraction(Axis::Vertical, bounds, at(0., 40.)), 0.);
        assert_eq!(fraction(Axis::Vertical, bounds, at(0., 10.)), 0.75);
    }

    #[test]
    fn steps_like_a_range_input() {
        assert_eq!(key_step("right", 1., 10., 0.5), Some(1.5));
        assert_eq!(key_step("down", 0.2, 10., 0.5), Some(0.));
        assert_eq!(key_step("pageup", 1., 100., 0.1), Some(11.));
        assert_eq!(key_step("end", 1., 7., 0.1), Some(7.));
        assert_eq!(key_step("a", 1., 7., 0.1), None);
    }
}
