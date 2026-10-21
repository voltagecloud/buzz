//! Checkbox and choice row (6.5).

use crate::{ActiveTheme, Icon, IconName, Surface, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, rems,
};
use std::rc::Rc;

/// Checkbox value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CheckboxState {
    #[default]
    Unchecked,
    Checked,
    Indeterminate,
}

impl From<bool> for CheckboxState {
    fn from(checked: bool) -> Self {
        if checked {
            Self::Checked
        } else {
            Self::Unchecked
        }
    }
}

type ChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// A 20×20 checkbox, optionally with a label and description (choice row).
#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    state: CheckboxState,
    label: Option<SharedString>,
    /// Shown in place of `label`, such as an avatar beside a name.
    label_element: Option<AnyElement>,
    description: Option<SharedString>,
    disabled: bool,
    surface: Surface,
    on_change: Option<ChangeHandler>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>, state: impl Into<CheckboxState>) -> Self {
        Self {
            id: id.into(),
            state: state.into(),
            label: None,
            label_element: None,
            description: None,
            disabled: false,
            surface: Surface::default(),
            on_change: None,
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// A label that is more than text (React's `label: ReactNode`).
    pub fn label_element(mut self, label: impl IntoElement) -> Self {
        self.label_element = Some(label.into_any_element());
        self
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
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

    /// Called with the new checked value.
    pub fn on_change(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Checkbox {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let ix = c.interaction(self.surface);
        let on = self.state != CheckboxState::Unchecked;
        let (bg, border, glyph) = match (self.disabled, on) {
            (true, _) => (
                c.affordance_disabled,
                c.affordance_disabled,
                c.text_unavailable,
            ),
            (false, true) => (
                c.affordance_prominent,
                c.affordance_prominent,
                c.text_inverse,
            ),
            (false, false) => (c.surface_panel, ix.border, c.text_inverse),
        };
        let mark = match self.state {
            CheckboxState::Unchecked => None,
            CheckboxState::Checked => Some(IconName::Check),
            CheckboxState::Indeterminate => Some(IconName::Minus),
        };
        let boxed = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(rems(1.25))
            .rounded(radius::CHIP)
            .border_1()
            .border_color(border)
            .bg(bg)
            .children(mark.map(|m| Icon::new(m).size(14.).color(glyph)));
        let has_text =
            self.label.is_some() || self.label_element.is_some() || self.description.is_some();
        let next = self.state != CheckboxState::Checked;
        div()
            .id(self.id)
            .flex()
            .gap(rems(0.75))
            .when(has_text, |this| this.min_h(rems(2.5)).items_start())
            .when(!self.disabled, |this| this.cursor_pointer())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .child(
                div()
                    .when(has_text, |this| this.pt(rems(0.625)))
                    .child(boxed),
            )
            .when(has_text, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .py(rems(0.625))
                        .when_some(self.label, |this, label| {
                            this.child(
                                div()
                                    .text_label_sm()
                                    .text_color(if self.disabled {
                                        c.text_unavailable
                                    } else {
                                        c.text_standard
                                    })
                                    .child(label),
                            )
                        })
                        .when_some(self.label_element, |this, label| {
                            this.child(
                                div()
                                    .text_label_sm()
                                    .text_color(if self.disabled {
                                        c.text_unavailable
                                    } else {
                                        c.text_standard
                                    })
                                    .child(label),
                            )
                        })
                        .when_some(self.description, |this, text| {
                            this.child(div().text_body_sm().text_color(c.text_subtle).child(text))
                        }),
                )
            })
            .when_some(self.on_change.filter(|_| !self.disabled), |this, f| {
                this.on_click(move |_, w, cx| f(next, w, cx))
            })
    }
}
