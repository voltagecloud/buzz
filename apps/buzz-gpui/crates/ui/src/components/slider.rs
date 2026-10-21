//! Stepped slider (`ComposerModels.module.css` `.slider`): a pill track
//! with a stop per value, an edge-aligned thumb, and an optional tab
//! attached to the track's top rim.

use crate::{ActiveTheme, TypeStyled, tokens::shadow};
use gpui_kit::{
    AnyElement, App, Bounds, CursorStyle, DispatchPhase, ElementId, FocusHandle, HitboxBehavior,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, PathBuilder, Pixels, RenderOnce, Styled, Window, canvas, div,
    point, prelude::FluentBuilder, px, relative,
};
use std::cell::RefCell;
use std::rc::Rc;

type ChangeHandler = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

/// The track's height: `--space-8 + --space-2`.
const TRACK: f32 = 40.;
/// The thumb's box; a 4px transparent border leaves a 24px dot.
const THUMB: f32 = 32.;
/// The tab's concave shoulders (`--shoulder`).
const SHOULDER: f32 = 8.;
/// The tab: 4px padding around a 16px caption line.
const TAB: f32 = 24.;

thread_local! {
    /// The slider a press started on, so a drag keeps moving it even when
    /// the pointer leaves its track.
    static DRAGGING: RefCell<Option<ElementId>> = const { RefCell::new(None) };
}

/// A slider over `stops` values, `0..stops`. Every press, drag step and
/// arrow key reports a value, even an unchanged one, so a tap on the
/// thumb's own stop still chooses it.
#[derive(IntoElement)]
pub struct Slider {
    id: ElementId,
    stops: usize,
    value: usize,
    unknown: bool,
    disabled: bool,
    focus: Option<FocusHandle>,
    tab: Option<AnyElement>,
    on_change: Option<ChangeHandler>,
}

impl Slider {
    pub fn new(id: impl Into<ElementId>, stops: usize, value: usize) -> Self {
        Self {
            id: id.into(),
            stops,
            value,
            unknown: false,
            disabled: false,
            focus: None,
            tab: None,
            on_change: None,
        }
    }

    /// The value is a placeholder: the thumb is muted and the fill hidden
    /// (`data-unknown`).
    pub fn unknown(mut self, unknown: bool) -> Self {
        self.unknown = unknown;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Focus for the arrow keys, Home and End.
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    /// Content of the tab joined to the track's top rim, at its end.
    pub fn tab(mut self, tab: impl IntoElement) -> Self {
        self.tab = Some(tab.into_any_element());
        self
    }

    pub fn on_change(mut self, f: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

/// The stop nearest `x` on a track at `bounds`, with the thumb aligned to
/// its edges: its centre runs from half a thumb in to half a thumb short.
fn stop_at(bounds: Bounds<Pixels>, x: Pixels, stops: usize) -> usize {
    let last = stops.saturating_sub(1);
    let span = (f32::from(bounds.size.width) - THUMB).max(1.);
    let fraction = ((f32::from(x - bounds.left()) - THUMB / 2.) / span).clamp(0., 1.);
    ((fraction * last as f32).round() as usize).min(last)
}

/// A range's keyboard step: arrows by one (up and right raise it), Page
/// Up and Down by ten, Home and End to the ends. `None` for other keys.
fn stop_key(key: &str, value: usize, stops: usize) -> Option<usize> {
    let last = stops.saturating_sub(1);
    Some(match key {
        "right" | "up" => (value + 1).min(last),
        "left" | "down" => value.saturating_sub(1),
        "pageup" => (value + 10).min(last),
        "pagedown" => value.saturating_sub(10),
        "home" => 0,
        "end" => last,
        _ => return None,
    })
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = *cx.colors();
        // The thumb's ring shows for keyboard focus only.
        let ring = self.focus.as_ref().is_some_and(|focus| {
            !self.disabled && focus.is_focused(window) && window.last_input_was_keyboard()
        });
        let last = self.stops.saturating_sub(1).max(1);
        let at = self.value.min(self.stops.saturating_sub(1));
        let fraction = at as f32 / last as f32;
        let interactive = !self.disabled && self.stops > 0;
        let thumb = if self.disabled {
            c.affordance_disabled
        } else if self.unknown {
            c.affordance_subtle
        } else {
            c.affordance_prominent
        };
        // A box a thumb narrower than the track: fractions of it place the
        // thumb's left edge and, half a thumb in, the stops.
        let lane = || {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right(px(THUMB))
        };
        let stops = (0..self.stops).map(|stop| {
            div()
                .absolute()
                .top(px((TRACK - 2.) / 2. - 2.))
                .left(relative(stop as f32 / last as f32))
                .ml(px(THUMB / 2. - 2.))
                .w(px(2.))
                .h(px(4.))
                .rounded_full()
                .bg(c.text_subtle)
        });
        // The fill runs from 4px in to under the thumb: the lane's fraction,
        // then a thumb-wide pill where the thumb sits.
        let fill = || {
            div()
                .absolute()
                .top(px((TRACK - 2. - 24.) / 2.))
                .h(px(24.))
                .rounded_full()
                .bg(c.affordance_subtle)
        };
        let indicator = lane()
            .child(fill().left(px(4.)).w(relative(fraction)))
            .child(fill().left(relative(fraction)).ml(px(4.)).w(px(THUMB - 8.)));
        let track = div()
            .relative()
            .w_full()
            .h(px(TRACK))
            .rounded_full()
            .border_1()
            .border_color(c.border_standard)
            .bg(c.surface_popover)
            .shadow(shadow::sm())
            .when(!self.unknown, |this| this.child(indicator))
            .child(lane().children(stops))
            .child(
                lane().child(
                    div()
                        .absolute()
                        .top(px((TRACK - 2. - THUMB) / 2.))
                        .left(relative(fraction))
                        .size(px(THUMB))
                        .p(px(4.))
                        .rounded_full()
                        .when(ring, |this| {
                            this.border_2().border_color(c.border_focus).p(px(2.))
                        })
                        .child(div().size_full().rounded_full().bg(thumb)),
                ),
            )
            .when_some(
                self.on_change.clone().filter(|_| interactive),
                |this, on_change| this.child(drag_area(self.id.clone(), self.stops, on_change)),
            );
        let stops_count = self.stops;
        div()
            .id(self.id.clone())
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .when(self.disabled, |this| this.cursor_not_allowed())
            .when(self.tab.is_some(), |this| this.pt(px(TAB)))
            .child(track)
            .children(self.tab.map(|tab| attached_tab(tab, &c)))
            .when_some(
                self.focus.filter(|_| interactive).zip(self.on_change),
                |this, (focus, on_change)| {
                    this.track_focus(&focus)
                        .on_key_down(move |event: &KeyDownEvent, window, cx| {
                            if let Some(next) = stop_key(&event.keystroke.key, at, stops_count) {
                                on_change(&next, window, cx);
                                cx.stop_propagation();
                            }
                        })
                },
            )
    }
}

/// The tab over the track's end, its bottom covering the track's top
/// stroke, with concave shoulders turning its sides into that stroke.
fn attached_tab(tab: AnyElement, c: &crate::Colors) -> impl IntoElement {
    let (fill, stroke) = (c.surface_popover, c.border_standard);
    let shoulder = move |left: bool| {
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                // The corner the arc turns about: the top outer corner.
                let (cx_, far) = if left {
                    (bounds.left(), bounds.right())
                } else {
                    (bounds.right(), bounds.left())
                };
                let top = bounds.top();
                let bottom = bounds.bottom();
                let arc = |radius: f32, steps: usize| {
                    (0..=steps).map(move |step| {
                        let angle = std::f32::consts::FRAC_PI_2 * step as f32 / steps as f32;
                        let dx = radius * angle.cos();
                        let dy = radius * angle.sin();
                        let x = if left { cx_ + px(dx) } else { cx_ - px(dx) };
                        point(x, top + px(dy))
                    })
                };
                // Fill outside the arc, toward the tab's foot.
                let mut path = PathBuilder::fill();
                path.move_to(point(far, bottom));
                for at in arc(SHOULDER, 12) {
                    path.line_to(at);
                }
                path.line_to(point(far, bottom));
                if let Ok(path) = path.build() {
                    window.paint_path(path, fill);
                }
                let mut path = PathBuilder::stroke(px(1.));
                for (index, at) in arc(SHOULDER - 0.5, 12).enumerate() {
                    if index == 0 {
                        path.move_to(at);
                    } else {
                        path.line_to(at);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, stroke);
                }
            },
        )
        .absolute()
        .bottom_0()
        .size(px(SHOULDER))
        .map(|this| {
            if left {
                this.right_full()
            } else {
                this.left_full()
            }
        })
    };
    let tab = div()
        .relative()
        .min_w_0()
        .h(px(TAB + 1.))
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .border_1()
        .border_b_0()
        .border_color(stroke)
        .rounded_t(crate::tokens::radius::ROW)
        .bg(fill)
        .text_caption()
        .text_color(c.text_standard)
        .whitespace_nowrap()
        .child(shoulder(true))
        .child(shoulder(false))
        .child(tab);
    // Inset half a track from each end, at the end.
    div()
        .absolute()
        .top_0()
        .left(px(TRACK / 2.))
        .right(px(TRACK / 2.))
        .flex()
        .justify_end()
        .child(tab)
}

/// Covers the slider's track and reports presses and drags on it.
fn drag_area(id: ElementId, stops: usize, on_change: ChangeHandler) -> impl IntoElement {
    canvas(
        move |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, _| {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            let (press_id, press) = (id.clone(), on_change.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                DRAGGING.with_borrow_mut(|dragging| *dragging = Some(press_id.clone()));
                press(&stop_at(bounds, event.position.x, stops), window, cx);
                cx.stop_propagation();
            });
            let drag_id = id.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                let ours = DRAGGING.with_borrow(|dragging| dragging.as_ref() == Some(&drag_id));
                if phase == DispatchPhase::Bubble && ours && event.dragging() {
                    on_change(&stop_at(bounds, event.position.x, stops), window, cx);
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

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui_kit::size;

    #[test]
    fn picks_the_nearest_stop_with_edge_aligned_thumbs() {
        // 32px thumb: centres run from 26 to 106 on a 10..122 track.
        let bounds = Bounds::new(point(px(10.), px(0.)), size(px(112.), px(40.)));
        assert_eq!(stop_at(bounds, px(0.), 3), 0);
        assert_eq!(stop_at(bounds, px(26.), 3), 0);
        assert_eq!(stop_at(bounds, px(66.), 3), 1);
        assert_eq!(stop_at(bounds, px(90.), 3), 2);
        assert_eq!(stop_at(bounds, px(500.), 3), 2);
        assert_eq!(stop_at(bounds, px(60.), 1), 0);
    }

    #[test]
    fn steps_like_a_range_input() {
        assert_eq!(stop_key("right", 0, 2), Some(1));
        assert_eq!(stop_key("right", 1, 2), Some(1));
        assert_eq!(stop_key("left", 0, 3), Some(0));
        assert_eq!(stop_key("up", 1, 3), Some(2));
        assert_eq!(stop_key("end", 0, 4), Some(3));
        assert_eq!(stop_key("home", 3, 4), Some(0));
        assert_eq!(stop_key("a", 1, 4), None);
    }
}
