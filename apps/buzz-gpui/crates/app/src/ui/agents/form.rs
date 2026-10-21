//! Layout shared by the agent forms.

use buzz_ui::{Colors, TypeStyled};
use gpui_kit::*;

/// `Field`: label, control, then the description (§6.3).
pub fn field(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    c: &Colors,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .min_w_0()
        .child(div().text_label_sm().child(label.into()))
        .child(control)
        .children(
            description.map(|text| div().text_body_sm().text_color(c.text_subtle).child(text)),
        )
}
