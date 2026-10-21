//! NavigationItem (6.11) and the channel sidebar row recipe (3.2, 3.3).

use super::ClickHandler;
use crate::{ActiveTheme, Icon, IconName, Surface, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, SharedString, StatefulInteractiveElement, Styled, StyledText,
    Window, div, prelude::FluentBuilder, rems,
};
use std::rc::Rc;

/// NavigationItem layouts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NavigationItemVariant {
    /// min-height 32, padding 4×16, gap 8, radius 8 (DS `row`).
    #[default]
    Row,
    /// Row with a 32px left inset.
    Inset,
    /// Padding 8, for listbox options.
    Option,
    /// Pill radius, padding-inline 16; selected = panel fill.
    Pill,
    /// The channel sidebar row: 28 high, body-sm in `text-navigation`,
    /// 20px icon box, 6px gap, quiet hover. The caller insets it 4px left
    /// and 10px right of the list content, as buzz-app paints the fill.
    Sidebar,
}

/// A navigation row: icon, label, trailing slot.
#[derive(IntoElement)]
pub struct NavigationItem {
    id: ElementId,
    label: SharedString,
    /// Replaces the plain label, such as one with matched letters
    /// underlined.
    styled_label: Option<StyledText>,
    variant: NavigationItemVariant,
    icon: Option<IconName>,
    icon_stroke: Option<f32>,
    leading: Option<AnyElement>,
    name_accessory: Option<AnyElement>,
    trailing: Option<AnyElement>,
    selected: bool,
    unread: bool,
    disabled: bool,
    surface: Surface,
    on_click: Option<ClickHandler>,
}

impl NavigationItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            styled_label: None,
            variant: NavigationItemVariant::default(),
            icon: None,
            icon_stroke: None,
            leading: None,
            name_accessory: None,
            trailing: None,
            selected: false,
            unread: false,
            disabled: false,
            surface: Surface::default(),
            on_click: None,
        }
    }

    /// Draws the label with its own runs, e.g. underlined matches.
    pub fn styled_label(mut self, label: StyledText) -> Self {
        self.styled_label = Some(label);
        self
    }

    pub fn variant(mut self, variant: NavigationItemVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Shorthand for the channel sidebar variant.
    pub fn sidebar(self) -> Self {
        self.variant(NavigationItemVariant::Sidebar)
    }

    /// Leading 16px icon (`Hash`, `Lock`, page icons).
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Sidebar page icons use stroke 2.5.
    pub fn icon_stroke(mut self, width: f32) -> Self {
        self.icon_stroke = Some(width);
        self
    }

    /// Leading element instead of an icon (DM avatar).
    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading = Some(element.into_any_element());
        self
    }

    /// Shown right after the label, which truncates first (a DM's status
    /// emoji).
    pub fn name_accessory(mut self, element: impl IntoElement) -> Self {
        self.name_accessory = Some(element.into_any_element());
        self
    }

    /// Trailing slot (`text-metadata`): badges, counts, dots.
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Sidebar unread label: `text-navigation-unread` at weight 600.
    pub fn unread(mut self, unread: bool) -> Self {
        self.unread = unread;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for NavigationItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        use NavigationItemVariant as V;
        let c = cx.colors();
        let sidebar = self.variant == V::Sidebar;
        let ix = c.interaction(if sidebar {
            Surface::Navigation
        } else {
            self.surface
        });
        let rest_text = if self.disabled {
            c.text_unavailable
        } else if sidebar && self.selected {
            c.text_standard
        } else if sidebar {
            c.text_navigation
        } else {
            c.text_standard
        };
        let selected_bg = match self.variant {
            V::Pill => c.surface_panel,
            _ => ix.selected,
        };
        let interactive = !self.disabled;
        let leading = self.leading.or_else(|| {
            self.icon.map(|icon| {
                let mut icon = Icon::new(icon).size(16.);
                if let Some(stroke) = self.icon_stroke {
                    icon = icon.stroke_width(stroke);
                }
                icon.into_any_element()
            })
        });
        let label_color =
            (sidebar && self.unread && !self.disabled).then_some(c.text_navigation_unread);
        let metadata = c.text_metadata;
        // Sidebar rows turn `text-standard` on hover.
        let hover_text = if sidebar { c.text_standard } else { rest_text };
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .text_color(rest_text)
            .map(|this| match self.variant {
                V::Row => this
                    .min_h(rems(2.))
                    .px(rems(1.))
                    .py(rems(0.25))
                    .gap(rems(0.5))
                    .rounded(radius::ROW)
                    .text_body(),
                V::Inset => this
                    .min_h(rems(2.))
                    .pl(rems(2.))
                    .pr(rems(1.))
                    .py(rems(0.25))
                    .gap(rems(0.5))
                    .rounded(radius::ROW)
                    .text_body(),
                V::Option => this
                    .min_h(rems(2.))
                    .p(rems(0.5))
                    .gap(rems(0.5))
                    .rounded(radius::ROW)
                    .text_body(),
                V::Pill => this
                    .min_h(rems(2.))
                    .px(rems(1.))
                    .py(rems(0.25))
                    .gap(rems(0.5))
                    .rounded_full()
                    .text_body(),
                V::Sidebar => this
                    .h(rems(1.75))
                    .pl(rems(0.25))
                    .pr(rems(0.375))
                    .py(rems(0.125))
                    .gap(rems(0.375))
                    .rounded(radius::ROW)
                    .text_body_sm(),
            })
            .when(self.selected, |this| this.bg(selected_bg))
            .when(interactive, |this| {
                this.cursor_pointer()
                    .hover(move |s| {
                        let s = s.text_color(hover_text);
                        if self.selected { s } else { s.bg(ix.row_hover) }
                    })
                    .when(!self.selected && !sidebar, |this| {
                        this.active(move |s| s.bg(ix.pressed))
                    })
            })
            .when(self.disabled, |this| this.cursor_not_allowed())
            .when_some(leading, |this, leading| {
                this.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .when(sidebar, |this| this.w(rems(1.25)).h(rems(1.5)))
                        .child(leading),
                )
            })
            .map(|this| {
                let label = div()
                    .min_w_0()
                    .truncate()
                    .when_some(label_color, |this, color| {
                        this.text_color(color).font_weight(FontWeight::SEMIBOLD)
                    })
                    .map(|this| match self.styled_label {
                        Some(styled) => this.child(styled),
                        None => this.child(self.label),
                    });
                match self.name_accessory {
                    None => this.child(label.flex_1()),
                    Some(accessory) => this.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(rems(0.5))
                            .child(label)
                            .child(div().flex().flex_none().child(accessory)),
                    ),
                }
            })
            .when_some(self.trailing, |this, trailing| {
                this.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(rems(0.5))
                        .text_color(metadata)
                        .when(sidebar, |this| this.pr(rems(0.25)))
                        .child(trailing),
                )
            })
            .when_some(self.on_click.filter(|_| interactive), |this, f| {
                this.on_click(move |e, w, cx| f(e, w, cx))
            })
    }
}
