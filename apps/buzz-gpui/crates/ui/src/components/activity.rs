//! Shared activity presentation (`TypingDots`, `ShimmerText`,
//! `AvatarStack`): the dots that breathe while an agent's request waits
//! and pulse in turn once it works, the sheen on a live step, and compact
//! stacked identity artwork. Callers own the evidence and the accessible
//! wording; reduced motion shows all of it still.

use super::{Avatar, AvatarSize};
use crate::{ActiveTheme, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::time::Duration;

/// Three dots: all breathing together over 2.4 s while `waiting`, or
/// fading in turn over 1.4 s, 0.2 s apart, while working. Both rest at
/// the breath's dim level, so waiting → working rises instead of flashing.
pub fn typing_dots(id: impl Into<ElementId>, waiting: bool, color: Hsla, cx: &App) -> AnyElement {
    let row = div().flex().flex_none().items_center().gap(px(2.));
    let dot = move |opacity: f32| div().size(px(4.)).rounded_full().bg(color).opacity(opacity);
    if cx.reduce_motion() {
        return row.children((0..3).map(|_| dot(1.))).into_any_element();
    }
    // 0.35 at the ends, 1 at the middle, eased both ways.
    let level = |phase: f32| 0.35 + 0.65 * ease_in_out(1. - (2. * phase - 1.).abs());
    let (period, id): (f32, ElementId) = if waiting {
        (
            2.4,
            ElementId::NamedChild(std::sync::Arc::new(id.into()), "waiting".into()),
        )
    } else {
        (
            1.4,
            ElementId::NamedChild(std::sync::Arc::new(id.into()), "working".into()),
        )
    };
    row.with_animation(
        id,
        Animation::new(Duration::from_secs_f32(period)).repeat(),
        move |row, delta| {
            row.children((0..3).map(|n| {
                if waiting {
                    return dot(level(delta));
                }
                dot(level((delta - n as f32 * 0.2 / period).rem_euclid(1.)))
            }))
        },
    )
    .into_any_element()
}

/// A short live-work line with a slow sheen between the subtle and
/// standard text colors, 2.4 s a pass. Plain text when not `active` or
/// under reduced motion.
pub fn shimmer_text(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> AnyElement {
    let text: SharedString = text.into();
    let c = *cx.colors();
    if !active || cx.reduce_motion() || text.is_empty() {
        return div().min_w_0().truncate().child(text).into_any_element();
    }
    let (subtle, standard) = (c.text_subtle, c.text_standard);
    let ranges: Vec<std::ops::Range<usize>> = text
        .char_indices()
        .map(|(at, ch)| at..at + ch.len_utf8())
        .collect();
    let count = ranges.len() as f32;
    div()
        .min_w_0()
        .truncate()
        .with_animation(
            id,
            Animation::new(Duration::from_millis(2400)).repeat(),
            move |line, delta| {
                // A 240%-wide gradient sliding from 100% to -100%.
                let position = 1. - 2. * delta;
                let highlights = ranges.iter().enumerate().map(|(index, range)| {
                    let x = (index as f32 + 0.5) / count;
                    let u = ((x + 1.4 * position) / 2.4).rem_euclid(1.);
                    let t = if (0.2..0.45).contains(&u) {
                        (u - 0.2) / 0.25
                    } else if (0.45..0.7).contains(&u) {
                        1. - (u - 0.45) / 0.25
                    } else {
                        0.
                    };
                    let color = mix(subtle, standard, t);
                    (
                        range.clone(),
                        HighlightStyle {
                            color: Some(color),
                            ..Default::default()
                        },
                    )
                });
                line.child(StyledText::new(text.clone()).with_highlights(highlights))
            },
        )
        .into_any_element()
}

fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (a.to_rgb(), b.to_rgb());
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}

/// [`avatar_stack`]'s two sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackSize {
    /// 21px artwork, a subtle "+N" beside it.
    Compact,
    /// 24px artwork in a 1.5px ring, a round "+N" badge: thread summaries.
    Small,
}

/// Up to three overlapping avatars and "+N" for the rest. The enclosing
/// control names them.
pub fn avatar_stack(
    id: impl Into<SharedString>,
    avatars: Vec<Avatar>,
    size: StackSize,
    cx: &App,
) -> Option<AnyElement> {
    if avatars.is_empty() {
        return None;
    }
    let id: SharedString = id.into();
    let c = *cx.colors();
    let small = size == StackSize::Small;
    let box_size = if small { 24. } else { 21. };
    let more = avatars.len().saturating_sub(3);
    let mut stack = div().flex().flex_none().items_center();
    for (index, avatar) in avatars.into_iter().take(3).enumerate() {
        let inner = if small { box_size - 3. } else { box_size };
        stack = stack.child(
            div()
                .id(SharedString::from(format!("{id}-{index}")))
                .flex_none()
                .size(px(box_size))
                .when(index > 0, |d| d.ml(px(-4.)))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .when(small, |d| d.bg(c.surface_panel))
                .child(avatar.size(AvatarSize::Px(inner))),
        );
    }
    if more > 0 {
        let count = div().text_caption().child(format!("+{more}"));
        stack = stack.child(if small {
            count
                .flex_none()
                .size(px(24.))
                .ml(px(-4.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(c.affordance_selected)
                .text_color(c.text_standard)
        } else {
            count.ml(px(8.)).text_color(c.text_subtle)
        });
    }
    Some(stack.into_any_element())
}

/// How an activity indicator arrives (`buzz-arrive`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrive {
    /// Inline beside text: a quiet fade at the state duration.
    Fade,
    /// A standalone pill: a slower settle as it opens its space.
    Grow,
}

/// Fades `element` in when it first appears; still under reduced motion.
pub fn arrive<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    element: E,
    how: Arrive,
    cx: &App,
) -> AnyElement {
    use crate::tokens::{duration, easing};
    if cx.reduce_motion() {
        return element.into_any_element();
    }
    let (length, curve) = match how {
        Arrive::Fade => (duration::STATE, easing::ENTER),
        Arrive::Grow => (duration::SETTLE, easing::SETTLE),
    };
    element
        .with_animation(
            id,
            Animation::new(length).with_easing(move |t| easing::cubic_bezier(curve, t)),
            |element, delta| element.opacity(delta),
        )
        .into_any_element()
}
