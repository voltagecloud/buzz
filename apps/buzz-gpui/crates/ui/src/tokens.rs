//! Non-color design tokens: spacing, sizes, radii, type ramp, elevation and
//! motion (`DS/styles/tokens.css`, `DS/styles/typography.css`).
//!
//! buzz-app writes these in `rem`, so they scale with the interface size
//! setting. They are [`Rems`] here for the same reason: GPUI resolves them
//! against the window's rem size, which [`crate::Theme`] sets to
//! `16px * interface_scale`.

use gpui_kit::{BoxShadow, FontWeight, Hsla, Rems, SharedString, hsla, point, px, rems};
use std::time::Duration;

/// Spacing scale (`--space-*`, section 1.5).
pub mod space {
    use super::*;

    pub const HALF: Rems = rems(0.125);
    pub const S1: Rems = rems(0.25);
    pub const S1H: Rems = rems(0.375);
    pub const S2: Rems = rems(0.5);
    pub const S3: Rems = rems(0.75);
    pub const S4: Rems = rems(1.);
    pub const S5: Rems = rems(1.25);
    pub const S6: Rems = rems(1.5);
    pub const S8: Rems = rems(2.);
    pub const S16: Rems = rems(4.);

    /// `--space-workspace-inset` (16).
    pub const WORKSPACE_INSET: Rems = S4;
    /// `--space-panel-gap` (8).
    pub const PANEL_GAP: Rems = S2;
    /// `--space-panel-inset` (24).
    pub const PANEL_INSET: Rems = S6;
    /// `--space-control-inset` (16).
    pub const CONTROL_INSET: Rems = S4;
    /// `--space-row-gap` (8).
    pub const ROW_GAP: Rems = S2;
    /// `--space-section-gap` (32).
    pub const SECTION_GAP: Rems = S8;
    /// `--space-page-section-gap` (64).
    pub const PAGE_SECTION_GAP: Rems = S16;
    /// `--space-chip-inset` (3).
    pub const CHIP_INSET: Rems = rems(0.1875);
    /// `--space-chip-gap` (4).
    pub const CHIP_GAP: Rems = S1;
}

/// Component sizes (`--size-*`).
pub mod size {
    use super::*;

    pub const TOPBAR: Rems = rems(3.5);
    /// The shell header actually uses `min-height: 3rem`.
    pub const SHELL_HEADER: Rems = rems(3.);
    pub const PANEL_HEADER: Rems = rems(2.5);
    pub const CONTROL_SM: Rems = rems(2.);
    pub const ROW: Rems = rems(2.);
    pub const CONTROL: Rems = rems(2.5);
    pub const CONTROL_LG: Rems = rems(3.25);
    pub const NAVIGATOR: Rems = rems(13.5);
    pub const RESIZE_SEAM: Rems = rems(0.875);
    pub const EMOJI_DISPLAY: Rems = rems(2.625);
    pub const EMOJI_LINE: Rems = rems(3.);
    pub const EMOJI_INLINE: Rems = rems(1.375);
    /// Community rail width.
    pub const RAIL: Rems = rems(3.);
}

/// Corner radii (`--radius-*`).
pub mod radius {
    use super::*;

    /// Inline chips, links, checkbox.
    pub const CHIP: Rems = rems(0.25);
    /// Rows, nav items, compact popups.
    pub const ROW: Rems = rems(0.5);
    /// IconButtons and nav tabs.
    pub const ICON_BUTTON: Rems = rems(0.625);
    /// Fields and `shape="control"` buttons.
    pub const CONTROL: Rems = rems(0.75);
    /// Floating surfaces, settings groups, empty state.
    pub const CONTAINER: Rems = rems(1.);
    /// Panels, dialogs, composer.
    pub const PANEL: Rems = rems(1.25);
    /// Text buttons (pill up to 52px).
    pub const CAPSULE: Rems = rems(1.625);
    /// Pills, circles, switches.
    pub const PILL: Rems = rems(624.9375);
    /// Default menu item radius: 16 - 4 - 1.
    pub const FLOATING_INNER: Rems = rems(0.6875);
    /// Compact menu item radius: 8 - 2 - 1.
    pub const FLOATING_INNER_COMPACT: Rems = rems(0.3125);
}

/// The two font families. Both are bundled (see [`crate::fonts`]).
pub mod font {
    pub const SANS: &str = "Inter";
    pub const MONO: &str = "JetBrains Mono";
}

/// Type roles (`typography.css:43-92`, section 1.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextRole {
    /// 56 / 61.6, 400.
    Display,
    /// 32 / 35.2, 500.
    Title,
    /// 24 / 26.4, 500.
    Heading,
    /// 20 / 28, 400.
    BodyLg,
    /// 16 / 24, 500.
    Label,
    /// 14 / 20, 400. The default body text.
    Body,
    /// 14 / 20, 400. Dense text, menus, descriptions.
    BodySm,
    /// 14 / 22.75 (1.625 leading), 400. Sent and received message prose,
    /// with a relaxed reading rhythm.
    Message,
    /// 14 / 20, 500. Button labels, field labels, row labels.
    LabelSm,
    /// 12 / 16, 400.
    Caption,
    /// 12 / 16, 400, JetBrains Mono.
    Mono,
}

impl TextRole {
    /// Font size.
    pub fn size(self) -> Rems {
        rems(match self {
            Self::Display => 3.5,
            Self::Title => 2.,
            Self::Heading => 1.5,
            Self::BodyLg => 1.25,
            Self::Label => 1.,
            Self::Body | Self::BodySm | Self::Message | Self::LabelSm => 0.875,
            Self::Caption | Self::Mono => 0.75,
        })
    }

    /// Line height.
    pub fn line_height(self) -> Rems {
        rems(match self {
            Self::Display => 3.85,
            Self::Title => 2.2,
            Self::Heading => 1.65,
            Self::BodyLg => 1.75,
            Self::Label => 1.5,
            Self::Body | Self::BodySm | Self::LabelSm => 1.25,
            Self::Message => 0.875 * 1.625,
            Self::Caption | Self::Mono => 1.,
        })
    }

    /// Font weight.
    pub fn weight(self) -> FontWeight {
        match self {
            Self::Title | Self::Heading | Self::Label | Self::LabelSm => FontWeight::MEDIUM,
            _ => FontWeight::NORMAL,
        }
    }

    /// Letter spacing in em. GPUI has no tracking control yet, so this is
    /// informational only.
    pub fn tracking_em(self) -> f32 {
        match self {
            Self::Display => -0.04,
            Self::Title => -0.015,
            Self::Heading => -0.0075,
            Self::BodyLg => -0.01,
            Self::Label => 0.,
            Self::Body => -0.005,
            Self::BodySm | Self::Message => -0.0025,
            Self::LabelSm => 0.0025,
            Self::Caption => 0.0133,
            Self::Mono => 0.03,
        }
    }

    /// Font family.
    pub fn family(self) -> SharedString {
        match self {
            Self::Mono => font::MONO.into(),
            _ => font::SANS.into(),
        }
    }
}

/// Elevation (`--shadow-*`, identical in both modes).
pub mod shadow {
    use super::*;

    fn black(alpha: f32) -> Hsla {
        hsla(0., 0., 0., alpha)
    }

    /// `0 1px 2px rgba(0,0,0,.05)`: panel, composer, switch thumb.
    pub fn xs() -> Vec<BoxShadow> {
        vec![BoxShadow::new(px(0.), px(1.), black(0.05)).blur_radius(px(2.))]
    }

    /// `0 0 2px rgba(0,0,0,.1), 0 4px 8px -2px rgba(0,0,0,.06)`: floating
    /// surfaces, dialogs, tooltips.
    pub fn sm() -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(0.), black(0.1)).blur_radius(px(2.)),
            BoxShadow::new(px(0.), px(4.), black(0.06))
                .blur_radius(px(8.))
                .spread_radius(px(-2.)),
        ]
    }

    /// `--shadow-panel`: workspace panels' translucent contour and subtle
    /// lift (`0 0 0 1px ring, 0 1px 2px -1px lift, 0 2px 4px soft`); dark
    /// mode keeps only the light rim, whose lift colors are transparent.
    pub fn panel(ring: Hsla, lift: Hsla, soft: Hsla) -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(0.), ring).spread_radius(px(1.)),
            BoxShadow::new(px(0.), px(1.), lift)
                .blur_radius(px(2.))
                .spread_radius(px(-1.)),
            BoxShadow::new(px(0.), px(2.), soft).blur_radius(px(4.)),
        ]
    }

    /// The glass rims: `inset 0 1px 0 rim-lit, inset 0 -1px 0 rim-shade`.
    pub fn rims(lit: Hsla, shade: Hsla) -> Vec<BoxShadow> {
        vec![
            BoxShadow {
                color: lit,
                offset: point(px(0.), px(1.)),
                blur_radius: px(0.),
                spread_radius: px(0.),
                inset: true,
            },
            BoxShadow {
                color: shade,
                offset: point(px(0.), px(-1.)),
                blur_radius: px(0.),
                spread_radius: px(0.),
                inset: true,
            },
        ]
    }
}

/// Motion durations (`--duration-*`).
pub mod duration {
    use super::*;

    pub const FAST: Duration = Duration::from_millis(120);
    pub const STATE: Duration = Duration::from_millis(150);
    pub const FIELD_FOCUS: Duration = STATE;
    pub const PANEL_ENTER: Duration = Duration::from_millis(180);
    pub const SETTLE: Duration = Duration::from_millis(220);
    pub const MENU_ENTER: Duration = Duration::from_millis(75);
    pub const MENU_EXIT: Duration = Duration::from_millis(60);
    pub const SPINNER_TURN: Duration = Duration::from_secs(1);
}

/// Easing curves (`--easing-*`) as cubic béziers `(x1, y1, x2, y2)`.
pub mod easing {
    /// `--easing-state`: CSS `ease`.
    pub const STATE: (f32, f32, f32, f32) = (0.25, 0.1, 0.25, 1.);
    /// `--easing-enter`.
    pub const ENTER: (f32, f32, f32, f32) = (0.23, 1., 0.32, 1.);
    /// `--easing-settle`.
    pub const SETTLE: (f32, f32, f32, f32) = (0.645, 0.045, 0.355, 1.);

    /// Evaluates a cubic bézier easing at `t` in `0..=1`.
    pub fn cubic_bezier(curve: (f32, f32, f32, f32), t: f32) -> f32 {
        let (x1, y1, x2, y2) = curve;
        let sample = |a1: f32, a2: f32, s: f32| {
            let u = 1. - s;
            3. * u * u * s * a1 + 3. * u * s * s * a2 + s * s * s
        };
        // Solve x(s) = t by bisection; 20 steps is far below a pixel.
        let (mut lo, mut hi) = (0f32, 1f32);
        for _ in 0..20 {
            let mid = (lo + hi) / 2.;
            if sample(x1, x2, mid) < t {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        sample(y1, y2, (lo + hi) / 2.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_ramp_matches_spec_in_px() {
        let px = |r: Rems| r.0 * 16.;
        assert_eq!(px(TextRole::Body.size()), 14.);
        assert_eq!(px(TextRole::Body.line_height()), 20.);
        assert_eq!(px(TextRole::Caption.line_height()), 16.);
        assert_eq!(px(TextRole::Message.size()), 14.);
        assert_eq!(px(TextRole::Message.line_height()), 22.75);
        assert!((px(TextRole::Display.line_height()) - 61.6).abs() < 0.01);
        assert!((px(TextRole::Heading.line_height()) - 26.4).abs() < 0.01);
        assert_eq!(px(radius::PANEL), 20.);
        assert_eq!(px(space::CHIP_INSET), 3.);
    }

    #[test]
    fn easing_endpoints() {
        assert!(easing::cubic_bezier(easing::SETTLE, 0.).abs() < 1e-3);
        assert!((easing::cubic_bezier(easing::SETTLE, 1.) - 1.).abs() < 1e-3);
    }
}
