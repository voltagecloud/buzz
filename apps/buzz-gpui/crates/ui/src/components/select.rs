//! Select trigger (6.4).
//!
//! Presentational, like [`super::Menu`]: the caller owns the open state and
//! shows a [`super::Menu`] of [`super::MenuItem`]s (with `checked`) below
//! the trigger while it is open.

use super::ClickHandler;
use super::text_field::field_colors;
use crate::{ActiveTheme, Icon, IconName, Surface, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, rems,
};
use std::rc::Rc;

/// A `variant="field"` select trigger: the Input recipe laid out
/// space-between with a trailing caret.
#[derive(IntoElement)]
pub struct Select {
    id: ElementId,
    value: Option<SharedString>,
    placeholder: SharedString,
    leading: Option<AnyElement>,
    open: bool,
    invalid: bool,
    disabled: bool,
    subtle: bool,
    surface: Surface,
    on_click: Option<ClickHandler>,
}

impl Select {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            value: None,
            placeholder: SharedString::default(),
            leading: None,
            open: false,
            invalid: false,
            disabled: false,
            subtle: false,
            surface: Surface::default(),
            on_click: None,
        }
    }

    /// The selected option's label.
    pub fn value(mut self, value: impl Into<SharedString>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// Shown in `text-metadata` while nothing is selected.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Content before the value, such as a logo.
    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading = Some(element.into_any_element());
        self
    }

    /// `data-popup-open`: the focus border and a flipped caret.
    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    pub fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Rests on the subtle affordance instead of the inset fill, as the
    /// Share dialog's fields do inside the elevated dialog.
    pub fn subtle(mut self) -> Self {
        self.subtle = true;
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Select {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (bg, border, text) =
            field_colors(self.open, self.invalid, self.disabled, self.surface, cx);
        let bg = if self.subtle && !self.disabled {
            cx.colors().affordance_subtle
        } else {
            bg
        };
        let metadata = cx.colors().text_metadata;
        let caret = if self.open {
            IconName::CaretUp
        } else {
            IconName::CaretDown
        };
        div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_between()
            .gap(rems(0.5))
            .w_full()
            .min_h(rems(2.5))
            .px(rems(1.))
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(text)
            .text_body()
            .when(!self.disabled, |this| this.cursor_pointer())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(rems(0.5))
                    .children(self.leading)
                    .child(match self.value {
                        Some(value) => div().min_w_0().truncate().child(value),
                        None => div()
                            .min_w_0()
                            .truncate()
                            .text_color(metadata)
                            .child(self.placeholder),
                    }),
            )
            .child(Icon::new(caret).size(16.))
            .when_some(self.on_click.filter(|_| !self.disabled), |this, f| {
                this.on_click(move |e, w, cx| f(e, w, cx))
            })
    }
}
