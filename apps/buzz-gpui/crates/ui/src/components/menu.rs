//! Menu surface and rows (6.7).
//!
//! These are presentational: open them in an anchored overlay (gpui
//! `anchored()` / `deferred()`, or a gpui-kit Popover) and drive the
//! highlight from keyboard state.

use super::ClickHandler;
use crate::{ActiveTheme, Icon, IconName, MaterialStyled, Surface, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rems,
};
use std::rc::Rc;

/// Menu density.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MenuSize {
    /// Radius 16, padding 4, rows 32 high with 11px corners.
    #[default]
    Default,
    /// Max width 448 instead of 320.
    Wide,
    /// Radius 8, padding 2, rows padded 4×8 with 5px corners.
    Compact,
}

/// The floating menu popup: a column of [`MenuItem`]s, [`MenuLabel`]s and
/// [`MenuSeparator`]s.
#[derive(IntoElement, Default)]
pub struct Menu {
    size: MenuSize,
    /// Grows to its content instead of stopping at 24rem: for menus whose
    /// every row must stay reachable (`maxHeight: var(--available-height)`).
    tall: bool,
    children: Vec<AnyElement>,
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn size(mut self, size: MenuSize) -> Self {
        self.size = size;
        self
    }

    pub fn tall(mut self) -> Self {
        self.tall = true;
        self
    }
}

impl ParentElement for Menu {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Menu {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let compact = self.size == MenuSize::Compact;
        div()
            .flex()
            .flex_col()
            .floating_surface(c)
            .text_body_sm()
            .min_w(rems(11.5))
            .max_w(rems(if self.size == MenuSize::Wide {
                28.
            } else {
                20.
            }))
            .when(!self.tall, |this| this.max_h(rems(24.)))
            .overflow_hidden()
            .when(compact, |this| this.rounded(radius::ROW).p(rems(0.125)))
            .when(!compact, |this| this.p(rems(0.25)))
            .children(self.children)
    }
}

/// One menu row.
#[derive(IntoElement)]
pub struct MenuItem {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    icon_size: f32,
    leading: Option<AnyElement>,
    trailing: Option<AnyElement>,
    checked: Option<bool>,
    submenu: bool,
    highlighted: bool,
    danger: bool,
    disabled: bool,
    size: MenuSize,
    on_click: Option<ClickHandler>,
}

impl MenuItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            icon_size: 16.,
            leading: None,
            trailing: None,
            checked: None,
            submenu: false,
            highlighted: false,
            danger: false,
            disabled: false,
            size: MenuSize::Default,
            on_click: None,
        }
    }

    /// Leading 16px icon in `text-subtle`.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Context menus draw 14px icons.
    pub fn icon_size(mut self, px: f32) -> Self {
        self.icon_size = px;
        self
    }

    /// Leading content in place of an icon, such as a person's avatar
    /// (`ChoiceRow` `leading`).
    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading = Some(element.into_any_element());
        self
    }

    /// Trailing content such as a [`super::Kbd`].
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }

    /// Reserves the trailing check slot; `true` shows the check.
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    /// Adds the trailing caret of a submenu parent.
    pub fn submenu(mut self) -> Self {
        self.submenu = true;
        self
    }

    /// Keyboard highlight or open submenu parent.
    pub fn highlighted(mut self, highlighted: bool) -> Self {
        self.highlighted = highlighted;
        self
    }

    /// `tone="danger"`.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn size(mut self, size: MenuSize) -> Self {
        self.size = size;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for MenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let compact = self.size == MenuSize::Compact;
        let ix = c.interaction(if compact {
            Surface::FloatingCompact
        } else {
            Surface::Floating
        });
        let text = if self.disabled {
            c.text_unavailable
        } else if self.danger {
            c.text_danger
        } else {
            c.text_standard
        };
        // Icons and supporting text are subtle at rest and turn standard
        // with the row highlight.
        let quiet = if self.disabled || self.danger {
            text
        } else {
            c.text_subtle
        };
        let lit = self.highlighted && !self.disabled;
        let group: SharedString = format!("menu-item-{:?}", self.id).into();
        let quiet_slot = |child: AnyElement| {
            let hover_text = text;
            div()
                .flex()
                .flex_none()
                .items_center()
                .text_color(if lit { text } else { quiet })
                .when(!self.disabled, |this| {
                    this.group_hover(group.clone(), move |s| s.text_color(hover_text))
                })
                .child(child)
        };
        let interactive = !self.disabled;
        div()
            .id(self.id)
            .group(group.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(rems(0.5))
            .min_h(rems(2.))
            .map(|this| {
                if compact {
                    this.px(rems(0.5))
                        .py(rems(0.25))
                        .rounded(radius::FLOATING_INNER_COMPACT)
                } else {
                    this.px(rems(0.75))
                        .py(rems(0.5))
                        .rounded(radius::FLOATING_INNER)
                }
            })
            .text_color(text)
            .when(lit, |this| this.bg(ix.row_hover))
            .when(interactive, |this| {
                this.cursor_pointer().hover(move |s| s.bg(ix.row_hover))
            })
            .when_some(self.icon, |this, icon| {
                this.child(quiet_slot(
                    Icon::new(icon).size(self.icon_size).into_any_element(),
                ))
            })
            .children(self.leading.map(quiet_slot))
            .child(div().flex_1().min_w_0().truncate().child(self.label))
            .children(self.trailing.map(quiet_slot))
            .when_some(self.checked, |this, checked| {
                this.child(div().flex_none().size(rems(1.)).when(checked, |this| {
                    this.child(Icon::new(IconName::Check).size(16.))
                }))
            })
            .when(self.submenu, |this| {
                this.child(quiet_slot(
                    Icon::new(IconName::CaretRight).size(16.).into_any_element(),
                ))
            })
            .when_some(self.on_click.filter(|_| interactive), |this, f| {
                this.on_click(move |e, w, cx| f(e, w, cx))
            })
    }
}

/// A group label: padding 8 8 4, `text-subtle` (quiet: `text-metadata`).
#[derive(IntoElement)]
pub struct MenuLabel {
    label: SharedString,
    quiet: bool,
}

impl MenuLabel {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            quiet: false,
        }
    }

    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
}

impl RenderOnce for MenuLabel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        div()
            .pt(rems(0.5))
            .px(rems(0.5))
            .pb(rems(0.25))
            .text_body_sm()
            .text_color(if self.quiet {
                c.text_metadata
            } else {
                c.text_subtle
            })
            .child(self.label)
    }
}

/// A 1px `border-standard` rule with 4px vertical margin.
#[derive(IntoElement, Default)]
pub struct MenuSeparator;

impl MenuSeparator {
    pub fn new() -> Self {
        Self
    }
}

impl RenderOnce for MenuSeparator {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .flex_none()
            .h(px(1.))
            .my(rems(0.25))
            .bg(cx.colors().border_standard)
    }
}
