//! The "Jump to latest" pill over a timeline or a thread
//! (`JumpToLatestButton`, ui.md §4.3).

use buzz_ui::tokens::{duration, easing, shadow};
use buzz_ui::{ActiveTheme, Icon, IconName, TypeStyled};
use gpui_kit::*;
use std::time::Instant;

/// The pill shows this far from the bottom.
pub const DISTANCE: Pixels = px(80.);
/// How far the pill rises above the composer when the composer shows an
/// agent activity pill (`2 * --space-6`).
const ACTIVITY_LANE: f32 = 48.;

/// Whether the pill shows, and the messages that arrived while it did.
#[derive(Default)]
pub struct Jump {
    shown: bool,
    /// When `shown` last changed, for the fade.
    since: Option<Instant>,
    /// Bumped on every change, so each fade starts over.
    generation: usize,
    /// Rows that arrived while the reader was away from the bottom.
    pub unseen: usize,
}

impl Jump {
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// Shows or hides the pill. Back at the bottom, the count resets.
    pub fn set(&mut self, shown: bool) {
        if !shown {
            self.unseen = 0;
        }
        if self.shown != shown {
            self.shown = shown;
            self.since = Some(Instant::now());
            self.generation += 1;
        }
    }

    /// Counts rows that arrived while the pill shows.
    pub fn arrived(&mut self, count: usize) {
        if self.shown {
            self.unseen += count;
        }
    }

    /// The pill, fading in over 150ms and out over 120ms, rising 2px as it
    /// appears. `lane` lifts it clear of an agent activity pill.
    /// `lane` lifts the pill over the composer's agent activity, and
    /// `inset` over a preview's Join prompt.
    pub fn render(
        &self,
        id: &'static str,
        lane: bool,
        inset: Pixels,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> Option<AnyElement> {
        let (fade, reduce) = (
            if self.shown {
                duration::STATE
            } else {
                duration::FAST
            },
            cx.reduce_motion(),
        );
        let since = self.since?;
        if !self.shown && (reduce || since.elapsed() >= fade) {
            return None;
        }
        let c = *cx.colors();
        let label = label(self.unseen);
        let mut pill = div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(16.))
            .rounded(px(12.))
            .bg(c.glass_4)
            .shadow({
                let mut shadows = shadow::sm();
                shadows.extend(shadow::rims(c.rim_lit, c.rim_shade));
                shadows
            })
            .text_label_sm()
            .text_color(c.text_standard)
            .child(Icon::new(IconName::ArrowDown).size(16.))
            .child(label);
        if self.shown {
            pill = pill
                .cursor_pointer()
                .hover(|s| s.bg(c.affordance_subtle_hover))
                .on_click(on_click);
        }
        let shown = self.shown;
        let curve = if shown { easing::SETTLE } else { easing::STATE };
        let pill: AnyElement = if reduce {
            pill.into_any_element()
        } else {
            pill.with_animation(
                (id, self.generation),
                Animation::new(fade),
                move |pill, delta| {
                    let t = easing::cubic_bezier(curve, delta);
                    let visible = if shown { t } else { 1. - t };
                    pill.opacity(visible).top(px(2. * (1. - visible)))
                },
            )
            .into_any_element()
        };
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(2.) + if lane { px(ACTIVITY_LANE) } else { inset })
                .flex()
                .justify_center()
                .child(pill)
                .into_any_element(),
        )
    }
}

/// "Jump to latest", or the count of messages that arrived meanwhile.
pub fn label(unseen: usize) -> String {
    match unseen {
        0 => "Jump to latest".to_string(),
        1 => "1 new message".to_string(),
        n => format!("{n} new messages"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn counts_only_while_shown_and_resets_at_the_bottom() {
        let mut jump = Jump::default();
        jump.arrived(2);
        assert_eq!(jump.unseen, 0);
        jump.set(true);
        jump.arrived(2);
        jump.arrived(1);
        assert_eq!(label(jump.unseen), "3 new messages");
        jump.set(false);
        assert_eq!(jump.unseen, 0);
        assert_eq!(label(1), "1 new message");
        assert_eq!(label(0), "Jump to latest");
    }
}
