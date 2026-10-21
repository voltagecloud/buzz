//! Resolved color roles for light and dark mode.
//!
//! Every field is a semantic role from buzz-app's design system
//! (`DS/styles/tokens.css`), resolved to its final literal. The CSS custom
//! property each field mirrors is listed next to it; the unit test in this
//! module checks every one of them against `docs/port/tokens-resolved.json`,
//! the values Chromium computed for the running web app.

use gpui_kit::{Hsla, Rgba};

/// Parses `#rrggbb`, `#rrggbbaa` or `rgba(r, g, b, a)` into a color.
///
/// Only used on the literals below, so malformed input is a programming
/// error and panics.
pub(crate) fn parse(literal: &str) -> Hsla {
    let rgba = if let Some(hex) = literal.strip_prefix('#') {
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex color") as f32;
        Rgba {
            r: byte(0) / 255.,
            g: byte(2) / 255.,
            b: byte(4) / 255.,
            a: if hex.len() == 8 { byte(6) / 255. } else { 1. },
        }
    } else {
        let inner = literal
            .strip_prefix("rgba(")
            .and_then(|s| s.strip_suffix(')'))
            .expect("rgba() color");
        let parts: Vec<f32> = inner
            .split(',')
            .map(|p| p.trim().parse().expect("rgba() component"))
            .collect();
        Rgba {
            r: parts[0] / 255.,
            g: parts[1] / 255.,
            b: parts[2] / 255.,
            a: parts[3],
        }
    };
    rgba.into()
}

macro_rules! colors {
    ($( $(#[$doc:meta])* $field:ident = $css:literal : $light:literal, $dark:literal; )*) => {
        /// Every semantic color role, resolved for one appearance.
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Colors {
            $( $(#[$doc])* pub $field: Hsla, )*
        }

        impl Colors {
            /// The light palette (`:root`).
            pub fn light() -> Self {
                Self { $( $field: parse($light), )* }
            }

            /// The dark palette (`:root[data-color-mode="dark"]`).
            pub fn dark() -> Self {
                Self { $( $field: parse($dark), )* }
            }
        }

        /// `(field, css variable, light, dark)` for the token test.
        #[cfg(test)]
        pub(crate) const TABLE: &[(&str, &str, &str, &str)] = &[
            $( (stringify!($field), $css, $light, $dark), )*
        ];
    };
}

colors! {
    // Surfaces
    /// Plain background behind panels; the window color.
    surface_base = "--surface-base": "#f5f5f6", "#000000";
    /// Opaque content panels, cards, timelines, composer.
    surface_panel = "--surface-panel": "#ffffff", "#1a1a1a";
    /// Menus and dialogs (opaque fallback of the elevated glass).
    surface_popover = "--surface-popover": "#ffffff", "#282828";
    /// Floating surfaces when blur is supported.
    surface_elevated_glass = "--surface-elevated-glass": "rgba(255, 255, 255, 0.9)", "rgba(40, 40, 40, 0.9)";
    /// Recessed regions: fields, code, unresolved chips.
    surface_inset = "--surface-inset": "#f5f5f6", "#101010";
    /// Image and video stage.
    surface_inverse = "--surface-inverse": "#191919", "#c7c7c7";
    /// Categorical purple fill.
    surface_categorical_purple = "--surface-categorical-purple": "#f7edfe", "#301c3b";
    /// Modal backdrop.
    bg_scrim = "--bg-scrim": "#00000060", "#000000a6";
    /// Selected pill inside glass chrome.
    bg_chrome_selected = "--bg-chrome-selected": "#ffffff", "#333333";

    // Text
    /// Primary text and inherited icons.
    text_standard = "--text-standard": "#0f0f0f", "#ffffff";
    /// Supporting text, author names, labels.
    text_subtle = "--text-subtle": "#525252", "#c7c7c7";
    /// Timestamps, counts, hints, placeholders.
    text_metadata = "--text-metadata": "#5f5f5f", "#a4a4a4";
    /// Disabled controls.
    text_unavailable = "--text-unavailable": "#959595", "#595959";
    /// Text on prominent fills and tooltips.
    text_inverse = "--text-inverse": "#ffffff", "#000000";
    /// Errors and destructive labels.
    text_danger = "--text-danger": "#641723", "#ffd1d9";
    /// Warnings.
    text_warning = "--text-warning": "#4f3422", "#ffe7b3";
    /// Success.
    text_success = "--text-success": "#193b2d", "#b1f1cb";
    /// Inline links and mentions.
    text_link = "--text-link": "#0b5fa8", "#83c4ff";
    /// Linked or selected identity text.
    text_accent = "--text-accent": "#402060", "#ecd9fa";
    /// Categorical purple, such as GitHub's merged state.
    text_categorical_purple = "--text-categorical-purple": "#402060", "#ecd9fa";
    /// Sidebar labels at rest.
    text_navigation = "--text-navigation": "#404040", "#c7c7c7";
    /// Unread sidebar labels.
    text_navigation_unread = "--text-navigation-unread": "#262626", "#ffffff";
    /// Text on `affordance_accent_prominent`.
    text_on_accent = "--text-on-accent": "#ffffff", "#ffffff";
    /// Icons on media glass.
    text_on_media = "--text-on-media": "#ffffff", "#ffffff";
    /// Top-bar profile initials on a white disc.
    text_avatar_fallback = "--text-avatar-fallback": "#525252", "#525252";

    // Borders
    /// Quiet separators, panel edges, floating surface edges.
    border_standard = "--border-standard": "#e8e8e8", "#232323";
    /// Control boundary: outline button, checkbox, focused field.
    border_prominent = "--border-prominent": "#808080", "#737373";
    /// Control boundary on floating surfaces.
    border_floating_control = "--border-floating-control": "#808080", "#808080";
    /// Keyboard focus rings, composer focus.
    border_focus = "--border-focus": "#000000", "#ffffff";
    /// Invalid field.
    border_danger = "--border-danger": "#e5484d", "#e5484d";
    /// Warning boundary.
    border_warning = "--border-warning": "#ab6400", "#ffc53d";
    /// Identity accent underline.
    border_accent = "--border-accent": "#be93e4", "#8457aa";
    /// Hairline around message images and videos.
    border_image = "": "rgba(0, 0, 0, 0.1)", "rgba(255, 255, 255, 0.1)";
    /// `--shadow-panel`'s 1px contour: a dark ring in light mode, a quiet
    /// light rim in dark mode.
    shadow_panel_ring = "": "rgba(0, 0, 0, 0.06)", "rgba(255, 255, 255, 0.08)";
    /// `--shadow-panel`'s lift (light mode only).
    shadow_panel_lift = "": "rgba(0, 0, 0, 0.06)", "rgba(0, 0, 0, 0)";
    shadow_panel_soft = "": "rgba(0, 0, 0, 0.04)", "rgba(0, 0, 0, 0)";
    /// Media-glass rim.
    border_media_glass = "--border-media-glass": "rgba(255, 255, 255, 0.1)", "rgba(255, 255, 255, 0.1)";

    // Affordances
    /// Primary action fill, checked switch and checkbox, tooltip background.
    affordance_prominent = "--affordance-prominent": "#000000", "#ffffff";
    affordance_prominent_hover = "--affordance-prominent-hover": "#191919", "#c7c7c7";
    affordance_prominent_pressed = "--affordance-prominent-pressed": "#525252", "#b8b8b8";
    /// Secondary fill, chips, avatar fallback.
    affordance_subtle = "--affordance-subtle": "#f5f5f6", "#232323";
    affordance_subtle_hover = "--affordance-subtle-hover": "#f1f1f2", "#2e2e2e";
    affordance_subtle_pressed = "--affordance-subtle-pressed": "#dadada", "#333333";
    /// Persistent selection.
    affordance_selected = "--affordance-selected": "#e8e8e8", "#333333";
    /// Disabled fill.
    affordance_disabled = "--affordance-disabled": "#e8e8e8", "#1a1a1a";
    /// Quiet navigation-row hover on a panel.
    affordance_panel_hover = "--affordance-panel-hover": "#f5f5f6", "#232323";
    /// Sidebar gentle hover and selection.
    affordance_navigation_quiet = "--affordance-navigation-quiet": "#f7f7f7", "#232323";
    /// Highlighted rows on floating surfaces.
    affordance_floating_hover = "--affordance-floating-hover": "#f5f5f6", "#333333";
    /// Selection in anchored popups.
    affordance_popover_selected = "--affordance-popover-selected": "#f5f5f6", "#404040";
    affordance_floating_control = "--affordance-floating-control": "#f5f5f6", "#404040";
    affordance_floating_control_hover = "--affordance-floating-control-hover": "#f1f1f2", "#595959";
    affordance_floating_control_pressed = "--affordance-floating-control-pressed": "#dadada", "#737373";
    affordance_floating_selected = "--affordance-floating-selected": "#dadada", "#404040";
    /// Top-bar ghost hover over the gradient.
    affordance_backdrop_hover = "--affordance-backdrop-hover": "rgba(0, 0, 0, 0.06)", "rgba(255, 255, 255, 0.1)";
    /// Top-bar pressed and selected.
    affordance_backdrop_pressed = "--affordance-backdrop-pressed": "rgba(0, 0, 0, 0.1)", "rgba(255, 255, 255, 0.16)";
    /// Destructive button fill.
    affordance_danger = "--affordance-danger": "#feebec", "#3b1219";
    affordance_danger_hover = "--affordance-danger-hover": "#ffdbdc", "#500f1c";
    affordance_danger_pressed = "--affordance-danger-pressed": "#ffcdce", "#611623";
    /// Warning fill (`.notice`).
    affordance_warning = "--affordance-warning": "#fff7c2", "#302008";
    /// Success fill.
    affordance_success = "--affordance-success": "#e6f6eb", "#132d21";
    /// Inline link hover background.
    affordance_link_hover = "--affordance-link-hover": "#e6f4fe", "#0d2847";
    /// Quiet identity fill, tint IconButton.
    affordance_accent = "--affordance-accent": "#f7edfe", "#301c3b";
    affordance_accent_hover = "--affordance-accent-hover": "#f2e2fc", "#3d224e";
    /// Identity accent fill (white text).
    affordance_accent_prominent = "--affordance-accent-prominent": "#8e4ec6", "#8e4ec6";
    affordance_accent_prominent_hover = "--affordance-accent-prominent-hover": "#8347b9", "#975acc";

    // Status
    /// Running activity, agent badge ink.
    status_online = "--status-online": "#218358", "#3dd68c";
    /// Online availability label tint.
    status_availability_online = "--status-availability-online": "#2b9a66", "#33b074";
    /// Online dot center.
    status_avatar_online = "--status-avatar-online": "#2b9a66", "#33b074";
    /// Online dot outline.
    status_avatar_online_border = "--status-avatar-online-border": "#218358", "#3dd68c";
    /// Away dot.
    status_away = "--status-away": "#ffba18", "#ffd60a";
    /// Offline dot.
    status_offline = "--status-offline": "#808080", "#a4a4a4";
    /// In-progress task icon.
    icon_status_progress = "--icon-status-progress": "#ffba18", "#ffd60a";
    /// Completed task icon.
    icon_status_complete = "--icon-status-complete": "#2b9a66", "#33b074";

    // Availability capsule fills (`interaction-availability`, section 1.4)
    availability_online_fill = "": "#ddeae5", "#14231c";
    availability_online_hover = "": "#d1e5dc", "#162d22";
    availability_online_pressed = "": "#c5dfd3", "#183628";
    availability_away_fill = "": "rgba(255, 186, 24, 0.12)", "rgba(255, 214, 10, 0.12)";
    availability_away_hover = "": "rgba(255, 186, 24, 0.18)", "rgba(255, 214, 10, 0.18)";
    availability_away_pressed = "": "rgba(255, 186, 24, 0.24)", "rgba(255, 214, 10, 0.24)";
    availability_offline_fill = "": "#e1e1e2", "#262626";
    availability_offline_hover = "": "#d8d8d8", "#313131";
    availability_offline_pressed = "": "#cececf", "#3c3c3c";

    // Media
    bg_media_glass = "--bg-media-glass": "rgba(0, 0, 0, 0.35)", "rgba(0, 0, 0, 0.35)";
    bg_media_canvas = "--bg-media-canvas": "#000000", "#000000";
    bg_media_track = "--bg-media-track": "rgba(255, 255, 255, 0.16)", "rgba(255, 255, 255, 0.16)";

    // Glass ramp and rims
    glass_1 = "--glass-1": "#ffffff5c", "#1c1c1c5c";
    /// Quiet glass (`--bg-glass-primary`): chrome tabs and nav.
    glass_2 = "--glass-2": "#ffffff85", "#1c1c1c85";
    /// `--bg-glass-primary-hover`.
    glass_3 = "--glass-3": "#ffffff9e", "#1c1c1ca8";
    /// Glass over glass (`--bg-glass-secondary`).
    glass_4 = "--glass-4": "#ffffffc7", "#1c1c1cc7";
    glass_5 = "--glass-5": "#ffffffe0", "#1c1c1ce0";
    /// Top inner highlight on glass.
    rim_lit = "--rim-lit": "#ffffff73", "#ffffff2e";
    /// Bottom inner highlight on glass.
    rim_shade = "--rim-shade": "#ffffff1f", "#ffffff0f";

    // Scrollbars
    /// The thumb every scroll container uses.
    scrollbar_thumb_quiet = "--scrollbar-thumb-quiet": "rgba(232, 232, 232, 0.9)", "rgba(35, 35, 35, 0.9)";
    /// Defined but unused by the universal recipe.
    scrollbar_thumb = "--scrollbar-thumb": "#808080", "#808080";

    // Accent ramps used directly by components
    /// ThemePicker accent bars and identity marks (purple-9).
    purple_9 = "--purple-9": "#8e4ec6", "#8e4ec6";
    /// Shell gradient fallback (`--workspace`).
    workspace = "--workspace": "#f5f5f6", "#000000";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a Chromium-resolved value: `rgb()`, `rgba()` or
    /// `color(srgb r g b / a)`.
    fn resolved(value: &str) -> Option<[f32; 4]> {
        let nums = |s: &str| -> Vec<f32> {
            s.split([',', ' ', '/'])
                .filter(|p| !p.is_empty())
                .map(|p| p.parse().unwrap())
                .collect()
        };
        if let Some(inner) = value.strip_prefix("rgba(").or(value.strip_prefix("rgb(")) {
            let n = nums(inner.trim_end_matches(')'));
            Some([
                n[0] / 255.,
                n[1] / 255.,
                n[2] / 255.,
                *n.get(3).unwrap_or(&1.),
            ])
        } else if let Some(inner) = value.strip_prefix("color(srgb ") {
            let n = nums(inner.trim_end_matches(')'));
            Some([n[0], n[1], n[2], *n.get(3).unwrap_or(&1.)])
        } else {
            None
        }
    }

    #[test]
    fn colors_match_resolved_tokens() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/port/tokens-resolved.json")).unwrap();
        let mut checked = 0;
        for (field, css, light, dark) in TABLE {
            if css.is_empty() {
                continue;
            }
            for (mode, literal) in [("light", light), ("dark", dark)] {
                let value = json["modes"][mode][css]["value"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{css} missing in {mode}"));
                let expected = resolved(value).unwrap();
                let ours: Rgba = parse(literal).to_rgb();
                let ours = [ours.r, ours.g, ours.b, ours.a];
                for (a, b) in ours.iter().zip(expected) {
                    assert!(
                        (a - b).abs() < 1.5 / 255.,
                        "{field} ({css}, {mode}): ours {literal}, browser {value}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 150, "only {checked} colors checked");
    }
}
