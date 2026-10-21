//! Radio choice row (`RadioGroup.tsx`, `forms.css` `.buzz-radio`).

use crate::{ActiveTheme, Surface, TypeStyled};
use gpui_kit::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, rems,
};
use std::rc::Rc;

type SelectHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// A 20px round radio with its label, in a 40px-high row that is
/// clickable as a whole. Group rows 12px apart.
#[derive(IntoElement)]
pub struct Radio {
    id: ElementId,
    label: SharedString,
    description: Option<SharedString>,
    checked: bool,
    disabled: bool,
    on_select: Option<SelectHandler>,
}

impl Radio {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, checked: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            checked,
            disabled: false,
            on_select: None,
        }
    }

    /// A quieter line under the label (`.buzz-choice-description`).
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_select(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Radio {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let ix = c.interaction(Surface::default());
        let (bg, border) = match (self.disabled, self.checked) {
            (true, _) => (c.affordance_disabled, c.affordance_disabled),
            (false, true) => (c.affordance_prominent, c.affordance_prominent),
            (false, false) => (c.surface_panel, ix.border),
        };
        let dot = if self.disabled {
            c.text_unavailable
        } else {
            c.text_inverse
        };
        div()
            .id(self.id)
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(2.5))
            .when(!self.disabled, |this| this.cursor_pointer())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(rems(1.25))
                    .rounded_full()
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .when(self.checked, |this| {
                        this.child(div().size(rems(0.5)).rounded_full().bg(dot))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_label_sm()
                            .text_color(if self.disabled {
                                c.text_unavailable
                            } else {
                                c.text_standard
                            })
                            .child(self.label),
                    )
                    .when_some(self.description, |this, description| {
                        this.child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(description),
                        )
                    }),
            )
            .when_some(self.on_select.filter(|_| !self.disabled), |this, f| {
                this.on_click(move |_, w, cx| f(w, cx))
            })
    }
}
