//! Button (6.1) and the variant fills shared with IconButton (6.2).

use super::{ClickHandler, TRANSPARENT, Tooltip};
use crate::{
    ActiveTheme, Colors, Icon, IconName, Spinner, Surface, TypeStyled,
    tokens::{TextRole, radius, shadow},
};
use gpui_kit::{
    App, BoxShadow, ClickEvent, ElementId, FocusHandle, Hsla, InteractiveElement, IntoElement,
    ParentElement, Rems, RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, rems,
};
use std::{rc::Rc, time::Duration};

/// Button and IconButton emphasis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonVariant {
    /// Black in light mode, white in dark: the main action in a group.
    Prominent,
    /// Quiet context fill. The Button default.
    #[default]
    Subtle,
    /// No fill at rest. The IconButton default.
    Ghost,
    /// Transparent with a 1px inset context border.
    Outline,
    /// Panel fill, for inverse surfaces.
    Inverted,
    /// Danger tint with danger text.
    Destructive,
    /// No fill; underlines on hover.
    Link,
    /// Purple identity tint (IconButton).
    Tint,
    /// `glass-primary` over the backdrop (IconButton).
    Chrome,
    /// Dark glass over media (IconButton).
    Media,
    /// No fill in any state; avatar triggers (IconButton).
    Avatar,
    /// Always transparent; the glyph carries the state (IconButton).
    Bare,
}

/// Resolved fills for one variant on one surface.
#[derive(Clone, Debug)]
pub(crate) struct Fills {
    pub rest: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub text: Hsla,
    /// Inset 1px stroke (outline, media rim).
    pub stroke: Option<Hsla>,
    pub shadow: Vec<BoxShadow>,
}

fn inset_stroke(color: Hsla) -> BoxShadow {
    BoxShadow::new(0.0.into(), 0.0.into(), color)
        .spread_radius(1.0.into())
        .inset()
}

impl Fills {
    pub fn resolve(
        variant: ButtonVariant,
        surface: Surface,
        selected: bool,
        disabled: bool,
        c: &Colors,
    ) -> Self {
        let ix = c.interaction(surface);
        let mut f = match variant {
            ButtonVariant::Prominent => Fills {
                rest: c.affordance_prominent,
                hover: c.affordance_prominent_hover,
                pressed: c.affordance_prominent_pressed,
                text: c.text_inverse,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Subtle => Fills {
                rest: ix.fill,
                hover: ix.hover,
                pressed: ix.pressed,
                text: c.text_standard,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Ghost => Fills {
                rest: TRANSPARENT,
                hover: ix.hover,
                pressed: ix.pressed,
                text: c.text_standard,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Outline => Fills {
                rest: TRANSPARENT,
                hover: ix.hover,
                pressed: ix.pressed,
                text: c.text_standard,
                stroke: Some(ix.border),
                shadow: vec![],
            },
            ButtonVariant::Inverted => Fills {
                rest: c.surface_panel,
                hover: ix.hover,
                pressed: ix.pressed,
                text: c.text_standard,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Destructive => Fills {
                rest: c.affordance_danger,
                hover: c.affordance_danger_hover,
                pressed: c.affordance_danger_pressed,
                text: c.text_danger,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Link | ButtonVariant::Avatar | ButtonVariant::Bare => Fills {
                rest: TRANSPARENT,
                hover: TRANSPARENT,
                pressed: TRANSPARENT,
                text: c.text_standard,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Tint => Fills {
                rest: c.affordance_accent,
                hover: c.affordance_accent_hover,
                pressed: c.affordance_accent_hover,
                text: c.text_accent,
                stroke: None,
                shadow: vec![],
            },
            ButtonVariant::Chrome => Fills {
                rest: c.glass_2,
                hover: c.glass_3,
                pressed: c.glass_3,
                text: c.text_standard,
                stroke: None,
                shadow: shadow::rims(c.rim_lit, c.rim_shade),
            },
            ButtonVariant::Media => Fills {
                rest: c.bg_media_glass,
                hover: c.bg_media_glass,
                pressed: c.bg_media_glass,
                text: c.text_on_media,
                stroke: Some(c.border_media_glass),
                shadow: vec![],
            },
        };
        if selected {
            match variant {
                ButtonVariant::Ghost => {
                    f.rest = ix.selected;
                    f.hover = ix.selected;
                }
                ButtonVariant::Chrome => {
                    f.rest = c.bg_chrome_selected;
                    f.hover = c.bg_chrome_selected;
                    f.shadow = shadow::xs();
                }
                ButtonVariant::Link | ButtonVariant::Avatar | ButtonVariant::Bare => {}
                _ => {
                    f.rest = f.pressed;
                    f.hover = f.pressed;
                }
            }
        }
        if disabled {
            f.text = c.text_unavailable;
            match variant {
                ButtonVariant::Ghost
                | ButtonVariant::Link
                | ButtonVariant::Avatar
                | ButtonVariant::Bare => f.rest = TRANSPARENT,
                ButtonVariant::Outline => {
                    f.rest = TRANSPARENT;
                    f.stroke = Some(c.border_standard);
                }
                ButtonVariant::Tint | ButtonVariant::Chrome | ButtonVariant::Media => {}
                _ => {
                    f.rest = c.affordance_disabled;
                    f.shadow = vec![];
                }
            }
            f.hover = f.rest;
            f.pressed = f.rest;
        }
        if let Some(stroke) = f.stroke {
            f.shadow.push(inset_stroke(stroke));
        }
        f
    }
}

/// Button sizes (min-height 24 / 32 / 40 / 52).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonSize {
    /// A caption-sized inline choice, such as the composer's model name:
    /// 24px high, 4px side padding and icon gap, 12px icons. Pair it with
    /// the ghost variant for an unframed label.
    Inline,
    Xs,
    /// Alias `compact`.
    Sm,
    #[default]
    Md,
    Lg,
}

impl ButtonSize {
    fn height(self) -> Rems {
        rems(match self {
            Self::Inline | Self::Xs => 1.5,
            Self::Sm => 2.,
            Self::Md => 2.5,
            Self::Lg => 3.25,
        })
    }

    fn padding_x(self) -> Rems {
        rems(match self {
            Self::Inline => 0.25,
            Self::Xs => 0.75,
            Self::Sm => 1.,
            Self::Md | Self::Lg => 1.5,
        })
    }

    fn padding_y(self) -> Rems {
        rems(match self {
            Self::Inline => 0.,
            Self::Xs => 0.25,
            Self::Sm => 0.375,
            Self::Md => 0.5,
            Self::Lg => 0.875,
        })
    }

    fn text(self) -> TextRole {
        match self {
            Self::Inline | Self::Xs => TextRole::Caption,
            _ => TextRole::LabelSm,
        }
    }

    fn icon(self) -> f32 {
        match self {
            Self::Inline => 12.,
            Self::Xs | Self::Sm => 16.,
            Self::Md | Self::Lg => 24.,
        }
    }

    fn gap(self) -> Rems {
        rems(if self == Self::Inline { 0.25 } else { 0.5 })
    }
}

/// Corner shape of a text button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonShape {
    /// 26px radius: a full pill at every size.
    #[default]
    Capsule,
    /// 12px radius (`shape="control"`).
    Control,
    /// 10px radius (thread-summary buttons).
    Row,
}

/// A single-line text action with optional leading and trailing icons.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    size: ButtonSize,
    shape: ButtonShape,
    surface: Surface,
    icon: Option<IconName>,
    icon_end: Option<IconName>,
    selected: bool,
    disabled: bool,
    loading: bool,
    full_width: bool,
    /// A compact select's trigger: the outline at `border-standard`.
    quiet: bool,
    tooltip: Option<SharedString>,
    label_color: Option<Hsla>,
    on_click: Option<ClickHandler>,
    focus: Option<FocusHandle>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::default(),
            size: ButtonSize::default(),
            shape: ButtonShape::default(),
            surface: Surface::default(),
            icon: None,
            icon_end: None,
            selected: false,
            disabled: false,
            loading: false,
            full_width: false,
            quiet: false,
            tooltip: None,
            label_color: None,
            on_click: None,
            focus: None,
        }
    }

    /// Recolours the text alone, such as an inline choice's subtle label
    /// beside a standard caret.
    pub fn label_color(mut self, color: impl Into<Hsla>) -> Self {
        self.label_color = Some(color.into());
        self
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

    pub fn prominent(self) -> Self {
        self.variant(ButtonVariant::Prominent)
    }

    pub fn ghost(self) -> Self {
        self.variant(ButtonVariant::Ghost)
    }

    pub fn outline(self) -> Self {
        self.variant(ButtonVariant::Outline)
    }

    /// A compact select's trigger (`Select variant="compact"`): an outline
    /// whose boundary stays quiet at `border-standard`; the value and caret
    /// identify it.
    pub fn compact_select(mut self) -> Self {
        self.quiet = true;
        self.variant(ButtonVariant::Outline)
    }

    pub fn destructive(self) -> Self {
        self.variant(ButtonVariant::Destructive)
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    pub fn shape(mut self, shape: ButtonShape) -> Self {
        self.shape = shape;
        self
    }

    /// The surface the button sits on (default panel). Floating surfaces
    /// use stronger dark-mode fills; the header uses translucent ones.
    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// Leading icon.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Trailing icon.
    pub fn icon_end(mut self, icon: IconName) -> Self {
        self.icon_end = Some(icon);
        self
    }

    /// Pressed / expanded (`aria-expanded`) look.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Hides the label (keeping its width) behind a spinner.
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    pub fn full_width(mut self) -> Self {
        self.full_width = true;
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

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let mut f = Fills::resolve(self.variant, self.surface, self.selected, self.disabled, c);
        if self.quiet && f.stroke.is_some() {
            f.stroke = Some(c.border_standard);
            f.shadow.pop();
            f.shadow.push(inset_stroke(c.border_standard));
        }
        let icon_size = self.size.icon();
        let interactive = !self.disabled && !self.loading;
        let focus_ring = c.border_focus;
        let link = self.variant == ButtonVariant::Link;
        let radius = match self.shape {
            ButtonShape::Capsule => radius::CAPSULE,
            ButtonShape::Control => radius::CONTROL,
            ButtonShape::Row => radius::ICON_BUTTON,
        };
        let label = div()
            .flex()
            .items_center()
            .gap(self.size.gap())
            .whitespace_nowrap()
            .when(self.loading, |this| this.opacity(0.))
            .when_some(self.icon, |this, icon| {
                this.child(Icon::new(icon).size(icon_size).color(f.text))
            })
            .child(match self.label_color.filter(|_| !self.disabled) {
                Some(color) => div().text_color(color).child(self.label).into_any_element(),
                None => self.label.into_any_element(),
            })
            .when_some(self.icon_end, |this, icon| {
                this.child(Icon::new(icon).size(icon_size).color(f.text))
            });
        div()
            .id(self.id)
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_h(self.size.height())
            .px(self.size.padding_x())
            .py(self.size.padding_y())
            .rounded(radius)
            .text_role(self.size.text())
            .text_color(f.text)
            .bg(f.rest)
            .shadow(f.shadow.clone())
            .when(self.full_width, |this| this.w_full())
            .when(self.disabled, |this| this.cursor_not_allowed())
            .when(interactive, |this| {
                this.cursor_pointer()
                    .hover(|s| {
                        let s = s.bg(f.hover);
                        if link { s.underline() } else { s }
                    })
                    .active(|s| s.bg(f.pressed))
            })
            .when(link && self.selected, |this| this.underline())
            .child(label)
            .when(self.loading, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Spinner::new().size(icon_size).color(f.text)),
                )
            })
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
