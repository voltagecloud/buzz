//! IconButton (6.2).

use super::{ClickHandler, Tooltip, button::Fills};
use crate::{ActiveTheme, ButtonVariant, Icon, IconName, Surface, tokens::radius};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Rems, RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, rems,
};
use std::{rc::Rc, time::Duration};

/// Square IconButton sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IconButtonSize {
    /// 20×20 box, 12px glyph.
    Xs,
    /// 32×32 box, 16px glyph (aliases `compact`, `toolbar`). The default:
    /// icon actions are compact unless they ask for more.
    #[default]
    Sm,
    /// 40×40 box, 24px glyph.
    Md,
    /// 52×52 box, 24px glyph.
    Lg,
}

impl IconButtonSize {
    fn box_size(self) -> Rems {
        rems(match self {
            Self::Xs => 1.25,
            Self::Sm => 2.,
            Self::Md => 2.5,
            Self::Lg => 3.25,
        })
    }

    fn icon(self, bare: bool) -> f32 {
        match self {
            Self::Xs => 12.,
            Self::Sm if bare => 20.,
            Self::Sm => 16.,
            Self::Md | Self::Lg => 24.,
        }
    }
}

/// IconButton corner shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IconButtonShape {
    /// 10px radius.
    #[default]
    Control,
    /// Full circle.
    Round,
}

/// A square button holding one glyph (or a custom child such as an
/// avatar).
#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: IconName,
    icon_size: Option<f32>,
    icon_color: Option<gpui_kit::Hsla>,
    stroke: Option<f32>,
    child: Option<AnyElement>,
    variant: ButtonVariant,
    size: IconButtonSize,
    shape: IconButtonShape,
    surface: Surface,
    selected: bool,
    disabled: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
    focus: Option<FocusHandle>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: IconName) -> Self {
        Self {
            id: id.into(),
            icon,
            icon_size: None,
            icon_color: None,
            stroke: None,
            child: None,
            variant: ButtonVariant::Ghost,
            size: IconButtonSize::default(),
            shape: IconButtonShape::default(),
            surface: Surface::default(),
            selected: false,
            disabled: false,
            tooltip: None,
            on_click: None,
            focus: None,
        }
    }

    /// Makes the button take keyboard focus through `focus`, with the
    /// focus ring while the keyboard drives it. A disabled button takes
    /// none.
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: IconButtonSize) -> Self {
        self.size = size;
        self
    }

    pub fn shape(mut self, shape: IconButtonShape) -> Self {
        self.shape = shape;
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// Overrides the glyph size the size class forces (e.g. 16px art in an
    /// `xs` tab close button).
    pub fn icon_size(mut self, px: f32) -> Self {
        self.icon_size = Some(px);
        self
    }

    /// Overrides the glyph color (e.g. `text-metadata` in the search
    /// capsule's clear button).
    pub fn icon_color(mut self, color: impl Into<gpui_kit::Hsla>) -> Self {
        self.icon_color = Some(color.into());
        self
    }

    pub fn stroke_width(mut self, width: f32) -> Self {
        self.stroke = Some(width);
        self
    }

    /// Replaces the glyph with arbitrary content (profile avatar).
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.child = Some(child.into_any_element());
        self
    }

    /// `data-selected`: ghost buttons keep the selected fill.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let mut f = Fills::resolve(self.variant, self.surface, self.selected, self.disabled, c);
        if let Some(color) = self.icon_color.filter(|_| !self.disabled) {
            f.text = color;
        }
        let focus_ring = c.border_focus;
        let bare = self.variant == ButtonVariant::Bare;
        let icon_size = self.icon_size.unwrap_or_else(|| self.size.icon(bare));
        let interactive = !self.disabled;
        let content = match self.child {
            Some(child) => child,
            None => {
                let mut icon = Icon::new(self.icon).size(icon_size).color(f.text);
                if let Some(stroke) = self.stroke {
                    icon = icon.stroke_width(stroke);
                }
                icon.into_any_element()
            }
        };
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(self.size.box_size())
            .map(|this| match self.shape {
                IconButtonShape::Control => this.rounded(radius::ICON_BUTTON),
                IconButtonShape::Round => this.rounded_full(),
            })
            .text_color(f.text)
            .bg(f.rest)
            .shadow(f.shadow.clone())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .when(interactive, |this| {
                this.cursor_pointer()
                    .hover(|s| s.bg(f.hover))
                    .active(|s| s.bg(f.pressed))
            })
            .child(content)
            .when_some(self.tooltip, |this, text| {
                this.tooltip(Tooltip::text(text))
                    .tooltip_show_delay(Duration::ZERO)
            })
            .when_some(self.on_click.filter(|_| interactive), |this, f| {
                this.on_click(move |e, w, cx| f(e, w, cx))
            })
            .when_some(self.focus.filter(|_| interactive), |this, focus| {
                this.track_focus(&focus)
                    .focus_visible(move |s| s.border_2().border_color(focus_ring))
            })
    }
}
