//! Shared pieces of the workspace's dialogs: the dismissal event, the
//! channel opener they call, and the result row used by the pickers.

use buzz_relay_client::{ChannelSummary, EventId};
use buzz_ui::{ActiveTheme, Icon, IconName, Surface, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// A dialog asks its host to close it.
pub struct Dismissed;

/// Opens a channel, optionally revealing a message in it.
pub type OpenChannel = Rc<dyn Fn(&ChannelSummary, Option<EventId>, &mut Window, &mut App)>;
/// Opens a conversation by ID once the channel list has it.
pub type OpenConversation = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Leading visual of a [`row`].
pub enum Leading {
    Icon(IconName),
    Element(AnyElement),
}

/// A picker row: min height 44, leading icon, label and a quieter detail
/// line (`SearchChoices.css`).
pub fn row(
    id: impl Into<ElementId>,
    leading: Leading,
    label: impl Into<SharedString>,
    detail: Option<SharedString>,
    highlighted: bool,
    cx: &App,
) -> Stateful<Div> {
    labeled_row(id, leading, label.into(), detail, highlighted, cx)
}

/// A [`row`] whose label is an element, such as a label with its matched
/// letters underlined (`search_kit::matched_label`).
pub fn labeled_row(
    id: impl Into<ElementId>,
    leading: Leading,
    label: impl IntoElement,
    detail: Option<SharedString>,
    highlighted: bool,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let ix = c.interaction(Surface::Floating);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(rems(0.75))
        .min_h(rems(2.75))
        .px(rems(1.))
        .py(rems(0.5))
        .rounded(px(11.))
        .cursor_pointer()
        .when(highlighted, |this| this.bg(ix.row_hover))
        .hover(move |s| s.bg(ix.row_hover))
        .child(match leading {
            Leading::Icon(icon) => div()
                .flex_none()
                .text_color(c.text_standard)
                .child(Icon::new(icon).size(16.))
                .into_any_element(),
            Leading::Element(element) => div().flex_none().child(element).into_any_element(),
        })
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_standard)
                        .truncate()
                        .child(label),
                )
                .children(detail.map(|detail| {
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .truncate()
                        .child(detail)
                })),
        )
}

/// A group heading inside a picker.
pub fn group_label(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px(rems(1.))
        .pt(rems(0.25))
        .pb(rems(0.25))
        .min_h(rems(1.75))
        .flex()
        .items_center()
        .text_body_sm()
        .text_color(cx.colors().text_subtle)
        .child(label.into())
}

/// A quiet status line inside a picker.
pub fn note(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px(rems(1.))
        .py(rems(0.5))
        .text_body_sm()
        .text_color(cx.colors().text_subtle)
        .child(text.into())
}

/// An inline error.
pub fn error(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_body_sm()
        .text_color(cx.colors().text_danger)
        .child(text.into())
}
