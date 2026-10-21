//! Popover surface (6.8).

use crate::{ActiveTheme, MaterialStyled, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    prelude::FluentBuilder, rems,
};

/// Popover widths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PopoverWidth {
    /// 320px.
    #[default]
    Default,
    /// 224px, with an 8px radius.
    Compact,
    /// 384px.
    Wide,
    /// Sized by its content.
    Auto,
}

/// Popover padding presets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PopoverPadding {
    /// 12px (`padding="content"`).
    #[default]
    Content,
    /// 4px (2px when compact) for lists.
    List,
    None,
}

/// The floating popover panel. Anchor it with gpui's `anchored()`
/// (bottom/start, 8px offset).
#[derive(IntoElement, Default)]
pub struct Popover {
    width: PopoverWidth,
    padding: PopoverPadding,
    title: Option<SharedString>,
    description: Option<SharedString>,
    children: Vec<AnyElement>,
}

impl Popover {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn width(mut self, width: PopoverWidth) -> Self {
        self.width = width;
        self
    }

    pub fn padding(mut self, padding: PopoverPadding) -> Self {
        self.padding = padding;
        self
    }

    /// Title in `text-label`.
    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Description in `text-body-sm` subtle, 8px under the title.
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl ParentElement for Popover {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Popover {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let compact = self.width == PopoverWidth::Compact;
        let padding = match (self.padding, compact) {
            (PopoverPadding::Content, _) => rems(0.75),
            (PopoverPadding::List, false) => rems(0.25),
            (PopoverPadding::List, true) => rems(0.125),
            (PopoverPadding::None, _) => rems(0.),
        };
        div()
            .flex()
            .flex_col()
            .floating_surface(c)
            .text_body()
            .p(padding)
            .map(|this| match self.width {
                PopoverWidth::Default => this.w(rems(20.)),
                PopoverWidth::Compact => this.w(rems(14.)).rounded(radius::ROW),
                PopoverWidth::Wide => this.w(rems(24.)),
                PopoverWidth::Auto => this,
            })
            .when_some(self.title, |this, title| {
                this.child(div().text_label().child(title))
            })
            .when_some(self.description, |this, text| {
                this.child(
                    div()
                        .mt(rems(0.5))
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child(text),
                )
            })
            .children(self.children)
    }
}
