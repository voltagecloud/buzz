//! SearchField (6.3): the default InputGroup look and the capsule used by
//! the mention and emoji pickers.

use super::TextField;
use crate::{ActiveTheme, Icon, IconButton, IconButtonSize, IconName, Surface, TypeStyled};
use gpui_kit::{
    App, Entity, IntoElement, ParentElement, RenderOnce, Styled, Window,
    component::input::{Input, InputState},
    div,
    prelude::FluentBuilder,
    rems,
};

/// A search input with a leading magnifier and a clear button that appears
/// once there is text.
#[derive(IntoElement)]
pub struct SearchField {
    state: Entity<InputState>,
    capsule: bool,
    invalid: bool,
    disabled: bool,
    surface: Surface,
}

impl SearchField {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            capsule: false,
            invalid: false,
            disabled: false,
            surface: Surface::default(),
        }
    }

    /// The pill variant: panel fill, prominent border, body-sm, no focus
    /// style.
    pub fn capsule(mut self) -> Self {
        self.capsule = true;
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

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }
}

impl RenderOnce for SearchField {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let has_text = !self.state.read(cx).value().is_empty();
        let state = self.state.clone();
        let clear = has_text.then(|| {
            IconButton::new("search-clear", IconName::X)
                .size(IconButtonSize::Sm)
                .when(self.capsule, |this| {
                    this.icon_color(cx.colors().text_metadata)
                })
                .surface(self.surface)
                .on_click(move |_, window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_value("", window, cx);
                        state.focus(window, cx);
                    });
                })
        });

        if !self.capsule {
            return TextField::new(&self.state)
                .invalid(self.invalid)
                .disabled(self.disabled)
                .surface(self.surface)
                .leading(Icon::new(IconName::MagnifyingGlass).size(16.))
                .when_some(clear, |this, clear| this.trailing(clear))
                .into_any_element();
        }

        let c = cx.colors();
        let metadata = c.text_metadata;
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .w_full()
            .min_h(rems(2.5))
            .pl(rems(1.))
            .pr(rems(if has_text { 0.25 } else { 1. }))
            .rounded_full()
            .border_1()
            .border_color(if self.invalid {
                c.border_danger
            } else {
                c.border_prominent
            })
            .bg(c.surface_panel)
            .text_body_sm()
            .text_color(c.text_standard)
            .when(self.disabled, |this| this.opacity(0.5))
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(16.)
                    .color(metadata),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.state)
                        .appearance(false)
                        .disabled(self.disabled)
                        .px_0()
                        .py_0()
                        .h(rems(1.25)),
                ),
            )
            .children(clear)
            .into_any_element()
    }
}
