//! Switch (6.5).

use crate::{ActiveTheme, TypeStyled, tokens::shadow};
use gpui_kit::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, rems,
};
use std::rc::Rc;

type ChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// A 36×20 track with a 14px thumb, optionally labelled.
#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    checked: bool,
    disabled: bool,
    label: Option<SharedString>,
    label_first: bool,
    on_change: Option<ChangeHandler>,
}

impl Switch {
    pub fn new(id: impl Into<ElementId>, checked: bool) -> Self {
        Self {
            id: id.into(),
            checked,
            disabled: false,
            label: None,
            label_first: false,
            on_change: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The label before the track, as buzz-app's `Switch` lays it out.
    pub fn label_first(mut self) -> Self {
        self.label_first = true;
        self
    }

    pub fn on_change(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let (track, border, thumb) = match (self.disabled, self.checked) {
            (true, _) => (
                c.affordance_selected,
                c.border_standard,
                c.affordance_subtle_pressed,
            ),
            (false, true) => (
                c.affordance_prominent,
                c.affordance_prominent,
                c.text_inverse,
            ),
            (false, false) => (c.affordance_subtle_hover, c.border_standard, c.text_inverse),
        };
        let switch = div()
            .flex()
            .flex_none()
            .items_center()
            .w(rems(2.25))
            .h(rems(1.25))
            .p(rems(0.125))
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(track)
            .child(
                div()
                    .size(rems(0.875))
                    .rounded_full()
                    .bg(thumb)
                    .when(!self.disabled, |this| this.shadow(shadow::xs()))
                    .when(self.checked, |this| this.ml(rems(1.))),
            );
        let next = !self.checked;
        div()
            .id(self.id)
            .flex()
            .items_center()
            .gap(rems(0.75))
            .text_body()
            .text_color(if self.disabled {
                c.text_unavailable
            } else {
                c.text_standard
            })
            .when(!self.disabled, |this| this.cursor_pointer())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .map(|this| {
                if self.label_first {
                    this.children(self.label).child(switch)
                } else {
                    this.child(switch).children(self.label)
                }
            })
            .when_some(self.on_change.filter(|_| !self.disabled), |this, f| {
                this.on_click(move |_, w, cx| f(next, w, cx))
            })
    }
}
