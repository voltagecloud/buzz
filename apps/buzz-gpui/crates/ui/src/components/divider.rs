//! Divider: the 1px `border-standard` hairline.

use crate::ActiveTheme;
use gpui_kit::{App, IntoElement, Rems, RenderOnce, Styled, Window, div, prelude::FluentBuilder};

/// A horizontal (default) or vertical 1px separator.
#[derive(IntoElement, Default)]
pub struct Divider {
    vertical: bool,
    inset: Option<Rems>,
}

impl Divider {
    pub fn horizontal() -> Self {
        Self::default()
    }

    pub fn vertical() -> Self {
        Self {
            vertical: true,
            inset: None,
        }
    }

    /// Insets both ends (e.g. 4px under a panel header).
    pub fn inset(mut self, inset: Rems) -> Self {
        self.inset = Some(inset);
        self
    }
}

impl RenderOnce for Divider {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = cx.colors().border_standard;
        div()
            .flex_none()
            .bg(color)
            // With an inset the line relies on the parent's stretch, since a
            // full-size line plus margins would overflow.
            .map(|this| match (self.vertical, self.inset.is_some()) {
                (true, false) => this.w(gpui_kit::px(1.)).h_full(),
                (true, true) => this.w(gpui_kit::px(1.)).self_stretch(),
                (false, false) => this.h(gpui_kit::px(1.)).w_full(),
                (false, true) => this.h(gpui_kit::px(1.)).self_stretch(),
            })
            .when_some(self.inset, |this, inset| {
                if self.vertical {
                    this.my(inset)
                } else {
                    this.mx(inset)
                }
            })
    }
}
