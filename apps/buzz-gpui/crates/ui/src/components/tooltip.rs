//! Tooltip (6.10).

use crate::{ActiveTheme, TypeStyled, tokens::shadow};
use gpui_kit::{
    AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, rems,
};

/// The tooltip bubble: caption text in a prominent-filled pill, padding
/// 4×8, radius 12, shadow-sm, max width 384.
///
/// ```ignore
/// div().id("x").tooltip(Tooltip::text("Search Buzz (Ctrl+K)"))
/// ```
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    /// A builder for GPUI's `.tooltip(...)`.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let text = text.into();
        move |_, cx| {
            let text = text.clone();
            cx.new(|_| Tooltip { text }).into()
        }
    }

    /// The bubble as an element, for custom placement and the gallery.
    pub fn bubble(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
        Self::surface(cx).child(text.into())
    }

    /// An empty bubble, for tooltips with more than text.
    pub fn surface(cx: &App) -> gpui_kit::Div {
        let c = cx.colors();
        div()
            .max_w(rems(24.))
            .px(rems(0.5))
            .py(rems(0.25))
            .rounded(rems(0.75))
            .bg(c.affordance_prominent)
            .text_color(c.text_inverse)
            .shadow(shadow::sm())
            .text_caption()
    }

    /// Tooltip content offset from the pointer: GPUI places tooltips at
    /// the pointer, and the margin keeps the bubble clear of the cursor.
    pub fn placed(content: impl IntoElement) -> gpui_kit::Div {
        div().pl(rems(0.5)).pt(rems(1.25)).child(content)
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Self::placed(Self::bubble(self.text.clone(), cx))
    }
}
