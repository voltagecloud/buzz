//! Form pieces Settings, the template views, the workflow editor and Me
//! share: a labelled field and lines of secondary or danger text.

use buzz_ui::{ActiveTheme, TypeStyled, tokens::space};
use gpui_kit::*;

/// How a [`field`] reads.
#[derive(Clone, Copy, Default)]
pub(crate) struct FieldStyle {
    /// Small body text for the description and error, as in the workflow
    /// forms; captions otherwise.
    pub body: bool,
    /// The description sits between the label and the control.
    pub description_first: bool,
    /// An error shows instead of the description, not after it.
    pub error_replaces: bool,
}

/// `Field`: a label, its control, and the description and error lines.
pub(crate) fn field(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    error: Option<SharedString>,
    control: impl IntoElement,
    style: FieldStyle,
    cx: &App,
) -> Div {
    let c = cx.colors();
    let text = |text: SharedString, color: Hsla| {
        let line = div().text_color(color).child(text);
        if style.body {
            line.text_body_sm()
        } else {
            line.text_caption()
        }
    };
    let description = match error {
        Some(_) if style.error_replaces => None,
        _ => description.map(|description| text(description, c.text_subtle)),
    };
    let error = error.map(|error| text(error, c.text_danger));
    let mut out = div().flex().flex_col().gap(space::S2).min_w_0().child(
        div()
            .text_label_sm()
            .text_color(c.text_standard)
            .child(label.into()),
    );
    if style.description_first {
        out = out.children(description).child(control);
    } else {
        out = out.child(control).children(description);
    }
    out.children(error)
}

/// A line of small text: secondary, or an error with `danger`.
pub(crate) fn line(text: impl Into<SharedString>, danger: bool, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .text_body_sm()
        .text_color(if danger { c.text_danger } else { c.text_subtle })
        .child(text.into())
}

/// A secondary paragraph (`text-body-sm text-secondary`).
pub(crate) fn note(text: impl Into<SharedString>, cx: &App) -> Div {
    line(text, false, cx)
}
