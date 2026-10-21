//! Styling helpers: type roles and the shared material recipes.

use crate::{Colors, tokens::TextRole, tokens::radius, tokens::shadow};
use gpui_kit::Styled;

/// Type-role shorthands for any styled element.
pub trait TypeStyled: Styled + Sized {
    /// Applies a type role's family, size, line height and weight.
    fn text_role(self, role: TextRole) -> Self {
        self.font_family(role.family())
            .text_size(role.size())
            .line_height(role.line_height())
            .font_weight(role.weight())
    }

    /// Draws emoji from the bundled colour font (see [`crate::fonts`]).
    /// Set once near the root; children inherit it.
    fn emoji_fallback(mut self) -> Self {
        self.text_style().font_fallbacks = Some(crate::fonts::fallbacks());
        self
    }

    fn text_display(self) -> Self {
        self.text_role(TextRole::Display)
    }
    fn text_title(self) -> Self {
        self.text_role(TextRole::Title)
    }
    fn text_heading(self) -> Self {
        self.text_role(TextRole::Heading)
    }
    fn text_body_lg(self) -> Self {
        self.text_role(TextRole::BodyLg)
    }
    fn text_label(self) -> Self {
        self.text_role(TextRole::Label)
    }
    fn text_body(self) -> Self {
        self.text_role(TextRole::Body)
    }
    fn text_body_sm(self) -> Self {
        self.text_role(TextRole::BodySm)
    }
    fn text_message(self) -> Self {
        self.text_role(TextRole::Message)
    }
    fn text_label_sm(self) -> Self {
        self.text_role(TextRole::LabelSm)
    }
    fn text_caption(self) -> Self {
        self.text_role(TextRole::Caption)
    }
    fn text_mono(self) -> Self {
        self.text_role(TextRole::Mono)
    }
}

impl<T: Styled> TypeStyled for T {}

/// The material recipes from `DS/styles/materials.css`.
pub trait MaterialStyled: Styled + Sized {
    /// `.panel`: opaque panel fill, radius 16, a transparent 1px border that
    /// keeps the geometry, and `shadow-panel` for the contour and lift.
    fn panel_surface(self, colors: &Colors) -> Self {
        self.bg(colors.surface_panel)
            .border_1()
            .border_color(gpui_kit::transparent_black())
            .rounded(radius::CONTAINER)
            .shadow(shadow::panel(
                colors.shadow_panel_ring,
                colors.shadow_panel_lift,
                colors.shadow_panel_soft,
            ))
    }

    /// `floating-surface`: menus, selects, popovers, toasts. Radius 16,
    /// 1px standard border, elevated glass (opaque popover fill, since GPUI
    /// has no backdrop blur), shadow-sm.
    fn floating_surface(self, colors: &Colors) -> Self {
        self.bg(colors.surface_popover)
            .border_1()
            .border_color(colors.border_standard)
            .rounded(radius::CONTAINER)
            .shadow(shadow::sm())
            .text_color(colors.text_standard)
    }

    /// `glass-primary`: chrome tabs, chrome nav, chrome IconButtons.
    fn glass_primary(self, colors: &Colors) -> Self {
        self.bg(colors.glass_2)
            .shadow(shadow::rims(colors.rim_lit, colors.rim_shade))
    }
}

impl<T: Styled> MaterialStyled for T {}
