//! Projects the Buzz theme onto gpui-kit's theme so kit components (inputs,
//! menus, scrollbars, the window root) use Buzz fonts, colors and rem size.

use crate::{Theme, tokens::font};
use gpui_kit::{
    App, Hsla,
    component::{Theme as KitTheme, ThemeMode},
    hsla, px,
};

/// Text selection highlight. buzz-app uses the platform `Highlight`; these
/// are Radix blue-5 in each mode, close to Chromium's default selection.
fn selection(dark: bool) -> Hsla {
    if dark {
        crate::colors::parse("#004074")
    } else {
        crate::colors::parse("#c2e5ff")
    }
}

pub(crate) fn sync(cx: &mut App) {
    let theme = Theme::base(cx).clone();
    let c = theme.colors;
    let scale = theme.interface_scale();
    let dark = theme.is_dark();
    // `change` reloads the registered kit palette for the mode; our values
    // go on top in a second edit, as `Theme::update` documents.
    KitTheme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    KitTheme::update(cx, |kit| {
        kit.font_family = font::SANS.into();
        kit.mono_font_family = font::MONO.into();
        // The kit Root sets the window rem size from this every frame.
        kit.font_size = theme.rem_size();
        kit.mono_font_size = px(12. * scale);
        kit.radius = px(12. * scale);
        kit.radius_lg = px(16. * scale);
        kit.shadow = true;

        kit.background = c.surface_panel;
        kit.foreground = c.text_standard;
        kit.border = c.border_standard;
        kit.input = c.border_prominent;
        kit.ring = c.border_focus;
        kit.caret = c.text_standard;
        kit.selection = selection(dark);
        kit.muted = c.affordance_subtle;
        kit.muted_foreground = c.text_metadata;
        kit.accent = c.affordance_floating_hover;
        kit.accent_foreground = c.text_standard;
        kit.popover = c.surface_popover;
        kit.popover_foreground = c.text_standard;
        kit.overlay = c.bg_scrim;
        kit.link = c.text_link;
        kit.link_hover = c.text_link;
        kit.link_active = c.text_link;

        kit.primary = c.affordance_prominent;
        kit.primary_hover = c.affordance_prominent_hover;
        kit.primary_active = c.affordance_prominent_pressed;
        kit.primary_foreground = c.text_inverse;
        kit.secondary = c.affordance_subtle;
        kit.secondary_hover = c.affordance_subtle_hover;
        kit.secondary_active = c.affordance_subtle_pressed;
        kit.secondary_foreground = c.text_standard;
        kit.danger = c.affordance_danger;
        kit.danger_hover = c.affordance_danger_hover;
        kit.danger_active = c.affordance_danger_pressed;
        kit.danger_foreground = c.text_danger;

        kit.list_hover = c.affordance_floating_hover;
        kit.list_active = c.affordance_popover_selected;
        kit.list_active_border = hsla(0., 0., 0., 0.);
        kit.switch = c.affordance_subtle_hover;
        kit.switch_thumb = c.text_inverse;
        kit.title_bar = hsla(0., 0., 0., 0.);
        kit.title_bar_border = hsla(0., 0., 0., 0.);
        kit.window_border = c.border_standard;
        kit.drag_border = c.border_focus;

        kit.scrollbar = hsla(0., 0., 0., 0.);
        kit.scrollbar_thumb = c.scrollbar_thumb_quiet;
        kit.scrollbar_thumb_hover = c.affordance_subtle_pressed;
    });
}
