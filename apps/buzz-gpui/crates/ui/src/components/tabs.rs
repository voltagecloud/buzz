//! Tabs (6.11).

use super::ClickHandler;
use crate::{
    ActiveTheme, ButtonVariant, Icon, IconButton, IconButtonSize, IconName, MaterialStyled,
    TypeStyled,
    tokens::{radius, shadow},
};
use gpui_kit::{
    App, ClickEvent, CursorStyle, Div, ElementId, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rems,
};
use std::rc::Rc;

/// Tab list styles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TabVariant {
    /// Glass pill on the backdrop (the top-bar "Me / Messages" nav).
    #[default]
    Chrome,
    /// Text tabs with a sliding 2px underline, on plain surfaces.
    Panel,
    /// Wrapping pill tabs.
    Workspace,
    /// Pane-header strip of content-sized rows, at most 192px wide, with
    /// close buttons.
    Navigation,
}

/// A host's own handlers added to a tab's element.
type Extend = Box<dyn FnOnce(Stateful<Div>) -> Stateful<Div>>;

/// One tab.
#[derive(IntoElement)]
pub struct Tab {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    selected: bool,
    preview: bool,
    variant: TabVariant,
    on_click: Option<ClickHandler>,
    on_close: Option<ClickHandler>,
    /// Navigation tabs a host lets the pointer drag (`data-draggable`).
    draggable: bool,
    /// The host's own handlers on the tab's element, such as a drag.
    extend: Option<Extend>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            selected: false,
            preview: false,
            variant: TabVariant::default(),
            on_click: None,
            on_close: None,
            draggable: false,
            extend: None,
        }
    }

    /// The pointer can drag the tab (`data-draggable`): it shows the grab
    /// cursor. The host adds the drag itself through [`Self::extend`].
    pub fn draggable(mut self, draggable: bool) -> Self {
        self.draggable = draggable;
        self
    }

    /// Adds the host's own handlers to the tab's element.
    pub fn extend(mut self, extend: impl FnOnce(Stateful<Div>) -> Stateful<Div> + 'static) -> Self {
        self.extend = Some(Box::new(extend));
        self
    }

    /// 16px icon (navigation tabs).
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Workspace preview tab (accent fill).
    pub fn preview(mut self, preview: bool) -> Self {
        self.preview = preview;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    /// Navigation tabs: adds the close button.
    pub fn on_close(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Tab {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let ix = c.interaction(crate::Surface::Panel);
        let selected = self.selected;
        let group: SharedString = format!("tab-{:?}", self.id).into();
        let base = div()
            .id(self.id)
            .group(group.clone())
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .cursor_pointer();
        let tab = match self.variant {
            TabVariant::Chrome => base
                .h(rems(2.))
                .px(rems(1.))
                .rounded_full()
                .text_body()
                .text_color(if selected {
                    c.text_standard
                } else {
                    c.text_subtle
                })
                .when(selected, |this| {
                    this.bg(c.bg_chrome_selected).shadow(shadow::xs())
                })
                .when(!selected, |this| {
                    this.hover(|s| s.bg(c.glass_3).text_color(c.text_standard))
                })
                .child(self.label),
            TabVariant::Panel => base
                .h(rems(2.25))
                .text_body()
                .text_color(if selected {
                    c.text_standard
                } else {
                    c.text_metadata
                })
                .hover(|s| s.text_color(c.text_standard))
                .child(self.label)
                .when(selected, |this| {
                    this.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(rems(0.25))
                            .h(px(2.))
                            .rounded_full()
                            .bg(c.text_standard),
                    )
                }),
            TabVariant::Workspace => base
                .px(rems(0.75))
                .py(rems(0.25))
                .rounded_full()
                .text_body()
                .text_color(if selected {
                    c.text_standard
                } else {
                    c.text_subtle
                })
                .when(self.preview, |this| this.bg(c.affordance_accent))
                .hover(|s| s.bg(c.affordance_selected).text_color(c.text_standard))
                .child(self.label),
            TabVariant::Navigation => {
                let closable = self.on_close.is_some();
                base.max_w(rems(12.))
                    .h(rems(2.))
                    .gap(rems(0.5))
                    .pl(rems(0.5))
                    .pr(rems(if closable { 2. } else { 0.5 }))
                    .rounded(radius::ICON_BUTTON)
                    .text_body_sm()
                    .text_color(c.text_standard)
                    .when(selected, |this| this.bg(c.affordance_subtle))
                    .when(!selected, |this| this.hover(|s| s.bg(ix.row_hover)))
                    .when_some(self.icon, |this, icon| {
                        this.child(Icon::new(icon).size(16.))
                    })
                    .child(div().min_w_0().truncate().child(self.label))
                    .when_some(self.on_close, |this, f| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right(rems(0.25))
                                .flex()
                                .items_center()
                                .when(!selected, |this| {
                                    this.opacity(0.)
                                        .group_hover(group.clone(), |s| s.opacity(1.))
                                })
                                .child(
                                    IconButton::new("close", IconName::X)
                                        .size(IconButtonSize::Xs)
                                        .icon_size(16.)
                                        .variant(ButtonVariant::Ghost)
                                        .on_click(move |e, w, cx| f(e, w, cx)),
                                ),
                        )
                    })
            }
        };
        let tab = tab
            .when(self.draggable, |this| this.cursor(CursorStyle::OpenHand))
            .when_some(self.on_click, |this, f| {
                this.on_click(move |e, w, cx| f(e, w, cx))
            });
        match self.extend {
            Some(extend) => extend(tab),
            None => tab,
        }
    }
}

/// A tab list.
#[derive(IntoElement)]
pub struct Tabs {
    variant: TabVariant,
    tabs: Vec<Tab>,
}

impl Tabs {
    pub fn new(variant: TabVariant) -> Self {
        Self {
            variant,
            tabs: Vec::new(),
        }
    }

    pub fn tab(mut self, mut tab: Tab) -> Self {
        tab.variant = self.variant;
        self.tabs.push(tab);
        self
    }
}

impl RenderOnce for Tabs {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let list = div().flex().items_center();
        match self.variant {
            TabVariant::Chrome => list
                .h(rems(2.5))
                .p(rems(0.25))
                .gap(rems(0.25))
                .rounded_full()
                .glass_primary(c),
            TabVariant::Panel => list.gap(rems(1.25)),
            TabVariant::Workspace => list.flex_wrap().gap(rems(0.5)),
            TabVariant::Navigation => list.h(rems(2.5)).pt(rems(0.25)).gap(rems(0.25)),
        }
        .children(self.tabs)
    }
}
