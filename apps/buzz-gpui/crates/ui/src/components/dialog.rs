//! Dialog, AlertDialog and the modal scrim (6.6).

use super::ClickHandler;
use crate::{
    ActiveTheme, IconButton, IconButtonSize, IconName, Surface, TypeStyled,
    tokens::{radius, shadow},
};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, rems,
};
use std::rc::Rc;

/// The full-window modal backdrop. Its children are centered.
#[derive(IntoElement, Default)]
pub struct Scrim {
    children: Vec<AnyElement>,
}

impl Scrim {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ParentElement for Scrim {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Scrim {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.colors().bg_scrim)
            .children(self.children)
    }
}

/// Dialog widths.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DialogSize {
    /// `min(496px, 100vw - 32px)`.
    #[default]
    Default,
    /// `min(896px, 100vw - 32px)`.
    Wide,
    /// `min(<px>, 100vw - 32px)`, for feature dialogs with their own width
    /// (the agent dialogs use 860 and 640).
    Px(f32),
}

fn popup(width: f32, cx: &App) -> gpui_kit::Div {
    let c = cx.colors();
    div()
        .flex()
        .flex_col()
        .w(rems(width / 16.))
        .max_w_full()
        .p(rems(1.5))
        .bg(c.surface_popover)
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::PANEL)
        .shadow(shadow::sm())
        .text_color(c.text_standard)
        .text_body()
}

/// A modal dialog: header (title, description, close), body, footer.
#[derive(IntoElement)]
pub struct Dialog {
    id: ElementId,
    title: SharedString,
    description: Option<SharedString>,
    size: DialogSize,
    body: Vec<AnyElement>,
    footer: Vec<AnyElement>,
    footer_leading: Vec<AnyElement>,
    header_actions: Vec<AnyElement>,
    compact_header_gap: bool,
    compact_footer_gap: bool,
    heading_title: bool,
    stable_height: bool,
    title_icon: Option<IconName>,
    title_leading: Option<AnyElement>,
    on_close: Option<ClickHandler>,
}

impl Dialog {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: None,
            size: DialogSize::default(),
            body: Vec::new(),
            footer: Vec::new(),
            footer_leading: Vec::new(),
            header_actions: Vec::new(),
            compact_header_gap: false,
            compact_footer_gap: false,
            heading_title: false,
            stable_height: false,
            title_icon: None,
            title_leading: None,
            on_close: None,
        }
    }

    /// An element before the title, such as a brand mark, 8px from it.
    pub fn title_leading(mut self, element: impl IntoElement) -> Self {
        self.title_leading = Some(element.into_any_element());
        self
    }

    /// A 16px icon before the title, 8px from it.
    pub fn title_icon(mut self, icon: IconName) -> Self {
        self.title_icon = Some(icon);
        self
    }

    /// Title in `text-heading` (24px) instead of `text-label`, as the
    /// agent dialogs use.
    pub fn heading_title(mut self) -> Self {
        self.heading_title = true;
        self
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// `height="stable"`: `min(36rem, 100dvh - 48px)` whatever the body
    /// holds, so switching views inside does not resize the dialog.
    pub fn stable_height(mut self) -> Self {
        self.stable_height = true;
        self
    }

    pub fn size(mut self, size: DialogSize) -> Self {
        self.size = size;
        self
    }

    /// Footer action, in order (Cancel before the primary action).
    pub fn footer(mut self, element: impl IntoElement) -> Self {
        self.footer.push(element.into_any_element());
        self
    }

    /// Footer action pinned to the start (`margin-right: auto`).
    pub fn footer_leading(mut self, element: impl IntoElement) -> Self {
        self.footer_leading.push(element.into_any_element());
        self
    }

    /// A header action, before the close button (gap 8).
    pub fn header_action(mut self, element: impl IntoElement) -> Self {
        self.header_actions.push(element.into_any_element());
        self
    }

    /// `headerGap="compact"`: 12px above the body instead of 24.
    pub fn compact_header_gap(mut self) -> Self {
        self.compact_header_gap = true;
        self
    }

    /// `footerGap="compact"`: 8px below the body instead of 24.
    pub fn compact_footer_gap(mut self) -> Self {
        self.compact_footer_gap = true;
        self
    }

    /// Shows the close button.
    pub fn on_close(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(f));
        self
    }
}

impl ParentElement for Dialog {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.body.extend(elements);
    }
}

impl RenderOnce for Dialog {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = *cx.colors();
        let width = match self.size {
            DialogSize::Default => 496.,
            DialogSize::Wide => 896.,
            DialogSize::Px(px) => px,
        };
        let has_footer = !self.footer.is_empty() || !self.footer_leading.is_empty();
        let heading = self.heading_title;
        // `max-height: 100dvh - 48px`; the body scrolls.
        let max_height = window.viewport_size().height - gpui_kit::px(48.);
        // Focus inside the dialog keeps Escape (`Cancel`) reaching it.
        let focus = window
            .use_keyed_state((self.id.clone(), "focus"), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let on_escape = self.on_close.clone();
        popup(width, cx)
            .id(self.id.clone())
            .key_context(crate::DIALOG_CONTEXT)
            .track_focus(&focus)
            .when_some(on_escape, |this, close| {
                this.on_action(move |_: &crate::Cancel, window, cx| {
                    close(&ClickEvent::default(), window, cx)
                })
            })
            .max_h(max_height)
            .when(self.stable_height, |this| {
                this.h((window.rem_size() * 36.).min(max_height))
            })
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(rems(1.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.5))
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(rems(0.5))
                                    .map(|this| {
                                        if heading {
                                            this.text_heading()
                                        } else {
                                            this.text_label()
                                        }
                                    })
                                    .children(self.title_leading)
                                    .when_some(self.title_icon, |this, icon| {
                                        this.child(crate::Icon::new(icon).size(16.))
                                    })
                                    .child(self.title),
                            )
                            .when_some(self.description, |this, text| {
                                this.child(
                                    div().text_body_sm().text_color(c.text_subtle).child(text),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .gap(rems(0.5))
                            .children(self.header_actions)
                            .when_some(self.on_close, |this, f| {
                                this.child(
                                    IconButton::new("dialog-close", IconName::X)
                                        .size(IconButtonSize::Sm)
                                        .surface(Surface::Floating)
                                        .on_click(move |e, w, cx| f(e, w, cx)),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .id("dialog-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pt(rems(if self.compact_header_gap { 0.75 } else { 1.5 }))
                    .pb(rems(if !has_footer {
                        0.
                    } else if self.compact_footer_gap {
                        0.5
                    } else {
                        1.5
                    }))
                    .children(self.body),
            )
            .when(has_footer, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap(rems(0.75))
                        .children(self.footer_leading)
                        .child(div().flex_1())
                        .children(self.footer),
                )
            })
    }
}

/// A confirmation dialog: heading-sized title, 16px gaps, no close button.
#[derive(IntoElement)]
pub struct AlertDialog {
    title: SharedString,
    description: Option<SharedString>,
    body: Vec<AnyElement>,
    actions: Vec<AnyElement>,
}

impl AlertDialog {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            description: None,
            body: Vec::new(),
            actions: Vec::new(),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// An action, in order (Cancel before the destructive action).
    pub fn action(mut self, element: impl IntoElement) -> Self {
        self.actions.push(element.into_any_element());
        self
    }
}

impl ParentElement for AlertDialog {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.body.extend(elements);
    }
}

impl RenderOnce for AlertDialog {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        popup(496., cx)
            .gap(rems(1.))
            .child(div().text_heading().child(self.title))
            .when_some(self.description, |this, text| {
                this.child(div().text_body_sm().text_color(c.text_subtle).child(text))
            })
            .children(self.body)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(rems(0.75))
                    .children(self.actions),
            )
    }
}
