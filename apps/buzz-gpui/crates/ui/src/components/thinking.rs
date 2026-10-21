//! The agent thinking badge (`ThinkingBadge.tsx`): one silhouette that
//! travels from the avatar's online corner to a Start-button-sized pill
//! under the portrait, in an 88px design canvas scaled to the avatar.
//! GPUI cannot clip an image to a path, so the notch is a ring in the
//! backdrop color, as the presence dot's is.

use crate::ActiveTheme;
use gpui_kit::{
    AnyElement, App, ElementId, Hsla, IntoElement, ParentElement, Styled, Window, div, px,
};
use std::time::{Duration, Instant};

/// The design canvas.
const CANVAS: f32 = 88.;
/// The online badge's center, and its size.
const CORNER: f32 = 74.8;
const DOT: f32 = 22.;
/// A compact (sm) Start button: the thinking pill's size.
const PILL: (f32, f32) = (64., 32.);
const MORPH: Duration = Duration::from_millis(300);

/// Where a morph is: it eases from `from` to the target since `since`.
struct Morph {
    thinking: bool,
    from: f32,
    dots_from: f32,
    since: Option<Instant>,
}

impl Morph {
    /// Shell and dots progress now, and whether the morph still runs.
    fn progress(&self) -> (f32, f32, bool) {
        let to = if self.thinking { 1. } else { 0. };
        let Some(since) = self.since else {
            return (to, to, false);
        };
        let time = (since.elapsed().as_secs_f32() / MORPH.as_secs_f32()).min(1.);
        let shell = self.from + (to - self.from) * (1. - (1. - time).powi(5));
        // The dots finish in a third of the time.
        let dots_time = (time * 3.).min(1.);
        let dots = self.dots_from + (to - self.dots_from) * (1. - (1. - dots_time).powi(3));
        (shell, dots, time < 1.)
    }
}

fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a = a.to_rgb();
    let b = b.to_rgb();
    gpui_kit::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}

/// The badge over an avatar of `size` px; `cutout` is the color behind it.
pub(crate) fn badge(
    id: ElementId,
    thinking: bool,
    size: f32,
    cutout: Option<Hsla>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = window.use_keyed_state(id, cx, |_, _| Morph {
        thinking,
        from: 0.,
        dots_from: 0.,
        // A badge that appears thinking grows out of the corner.
        since: thinking.then(Instant::now),
    });
    if state.read(cx).thinking != thinking {
        state.update(cx, |morph, _| {
            let (shell, dots, _) = morph.progress();
            morph.from = shell;
            morph.dots_from = dots;
            morph.thinking = thinking;
            morph.since = Some(Instant::now());
        });
    }
    let (p, dots, running) = state.read(cx).progress();
    if running {
        window.request_animation_frame();
    }
    let c = cx.colors();
    let pixels = size.max(1.);
    let k = pixels / CANVAS;
    let scale = (CANVAS / pixels).min(1.).sqrt();
    let gap = (6.6_f32).min(3. * CANVAS / pixels);
    let thinking_gap = gap * (0.75 + 0.25 * ((size - 40.) / 40.).clamp(0., 1.));
    let (pill_w, pill_h) = (PILL.0 * scale, PILL.1 * scale);
    let x = CORNER + (CANVAS / 2. - CORNER) * p;
    let y = CORNER + (CANVAS - CORNER) * p;
    let w = DOT + (pill_w - DOT) * p;
    let h = DOT + (pill_h - DOT) * p;
    let radius = 7. + (pill_h / 2. - 7.) * p;
    let notch = gap + (thinking_gap - gap) * p;
    let ink = mix(c.status_online, c.affordance_prominent, p);
    let dot = 7. * scale * k;
    let ring = div()
        .absolute()
        .left(px((x - w / 2. - notch) * k))
        .top(px((y - h / 2. - notch) * k))
        .w(px((w + 2. * notch) * k))
        .h(px((h + 2. * notch) * k))
        .rounded(px((radius + notch) * k))
        .bg(cutout.unwrap_or(c.surface_panel));
    let dot_color = c.text_inverse;
    let pill = div()
        .absolute()
        .left(px((x - w / 2.) * k))
        .top(px((y - h / 2.) * k))
        .w(px(w * k))
        .h(px(h * k))
        .rounded(px(radius * k))
        .bg(ink)
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .when_dots(dots, dot, scale * k, thinking, dot_color);
    div()
        .absolute()
        .top_0()
        .left_0()
        .size(px(pixels))
        .child(ring)
        .child(pill)
        .into_any_element()
}

trait Dots {
    fn when_dots(self, opacity: f32, dot: f32, scale: f32, pulsing: bool, color: Hsla) -> Self;
}

impl Dots for gpui_kit::Div {
    /// Three dots pulsing 80 ms apart over 1.2 s (`badge-pill-typing-pulse`).
    fn when_dots(self, opacity: f32, dot: f32, scale: f32, pulsing: bool, color: Hsla) -> Self {
        if opacity <= 0.01 {
            return self;
        }
        use gpui_kit::{Animation, AnimationExt};
        let row = div()
            .flex()
            .items_center()
            .gap(px(4. * scale))
            .opacity(opacity);
        let dots = move |row: gpui_kit::Div, delta: f32| {
            row.children((0..3).map(|n| {
                let phase = (delta - n as f32 * 80. / 1200.).rem_euclid(1.);
                // 0–40% and 100% dim; 20% full and 2px up.
                let lift = if pulsing && phase < 0.4 {
                    1. - ((phase - 0.2).abs() / 0.2)
                } else {
                    0.
                };
                div()
                    .size(px(dot))
                    .rounded_full()
                    .bg(color)
                    .opacity(0.4 + 0.6 * lift)
                    .mt(px(-2. * scale * lift))
            }))
        };
        self.child(row.with_animation(
            "thinking-dots",
            Animation::new(Duration::from_millis(1200)).repeat(),
            dots,
        ))
    }
}
