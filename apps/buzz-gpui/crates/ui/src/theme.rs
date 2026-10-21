//! The global theme: appearance preference, resolved palette and interface
//! size, plus the projection onto gpui-kit's theme.

use crate::colors::Colors;
use gpui_kit::{App, Global, Hsla, Pixels, Subscription, Window, WindowAppearance, px};

/// Light or dark, after resolving the preference against the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Appearance {
    #[default]
    Light,
    Dark,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }
}

impl From<WindowAppearance> for Appearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }
}

/// The user's appearance setting (`buzz-appearance.v1`). Default `System`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AppearancePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// Which painted surface a control sits on (section 1.4). Hover, pressed and
/// selected fills depend on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Opaque panels; the default.
    #[default]
    Panel,
    /// Dialogs, menus, selects, popovers, toasts.
    Floating,
    /// Compact menus and popovers (`data-size="compact"`).
    FloatingCompact,
    /// The shell header over the backdrop gradient.
    Header,
    /// The channel sidebar (`.sidebar` overrides).
    Navigation,
}

/// The `--interaction-*` variables for one [`Surface`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interaction {
    /// Subtle control fill at rest.
    pub fill: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub selected: Hsla,
    /// Control boundary (outline buttons, checkbox, focused field).
    pub border: Hsla,
    pub row_hover: Hsla,
    pub row_text: Hsla,
    pub row_metadata: Hsla,
    pub selected_text: Hsla,
    pub selected_metadata: Hsla,
}

impl Colors {
    /// Resolves the interaction context for a surface.
    pub fn interaction(&self, surface: Surface) -> Interaction {
        let panel = Interaction {
            fill: self.affordance_subtle,
            hover: self.affordance_subtle_hover,
            pressed: self.affordance_subtle_pressed,
            selected: self.affordance_selected,
            border: self.border_prominent,
            row_hover: self.affordance_panel_hover,
            row_text: self.text_subtle,
            row_metadata: self.text_metadata,
            selected_text: self.text_subtle,
            selected_metadata: self.text_metadata,
        };
        let floating = Interaction {
            fill: self.affordance_floating_control,
            hover: self.affordance_floating_control_hover,
            pressed: self.affordance_floating_control_pressed,
            selected: self.affordance_floating_selected,
            border: self.border_floating_control,
            row_hover: self.affordance_floating_hover,
            row_text: self.text_standard,
            row_metadata: self.text_standard,
            selected_text: self.text_standard,
            selected_metadata: self.text_standard,
        };
        match surface {
            Surface::Panel => panel,
            Surface::Floating => floating,
            Surface::FloatingCompact => Interaction {
                row_hover: self.affordance_subtle_hover,
                selected: self.affordance_subtle_pressed,
                ..floating
            },
            Surface::Header => Interaction {
                hover: self.affordance_backdrop_hover,
                pressed: self.affordance_backdrop_pressed,
                selected: self.affordance_backdrop_pressed,
                ..panel
            },
            Surface::Navigation => Interaction {
                row_hover: self.affordance_navigation_quiet,
                ..panel
            },
        }
    }
}

/// Interface size bounds and step (`buzz-font-scale.v1`).
pub const INTERFACE_SCALE_MIN: f32 = 0.8;
pub const INTERFACE_SCALE_MAX: f32 = 2.0;
pub const INTERFACE_SCALE_STEP: f32 = 0.1;

/// The Buzz theme, a GPUI global. Read it with [`ActiveTheme::theme`].
#[derive(Clone, Debug)]
pub struct Theme {
    preference: AppearancePreference,
    system: Appearance,
    appearance: Appearance,
    interface_scale: f32,
    /// The palette for the current appearance.
    pub colors: Colors,
}

/// The base theme plus any [`crate::ThemeScope`] overrides being laid out.
pub(crate) struct ThemeStack(pub(crate) Vec<Theme>);

impl Global for ThemeStack {}

/// Replaced on every edit of the app theme, so observers hear about real
/// changes only (scopes also touch [`ThemeStack`] while laying out).
struct ThemeEdited;

impl Global for ThemeEdited {}

impl Default for Theme {
    fn default() -> Self {
        Self {
            preference: AppearancePreference::System,
            system: Appearance::Light,
            appearance: Appearance::Light,
            interface_scale: 1.,
            colors: Colors::light(),
        }
    }
}

impl Theme {
    /// The active theme: the innermost [`crate::ThemeScope`] being laid out,
    /// or the app theme.
    pub fn global(cx: &App) -> &Self {
        cx.global::<ThemeStack>()
            .0
            .last()
            .expect("buzz_ui::init was not called")
    }

    /// The app theme, ignoring scopes.
    pub fn base(cx: &App) -> &Self {
        &cx.global::<ThemeStack>().0[0]
    }

    /// A copy of this theme forced to one appearance.
    pub fn scoped(&self, appearance: Appearance) -> Self {
        Self {
            appearance,
            colors: Self::colors_for(appearance),
            ..self.clone()
        }
    }

    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    pub fn is_dark(&self) -> bool {
        self.appearance.is_dark()
    }

    pub fn preference(&self) -> AppearancePreference {
        self.preference
    }

    /// The interface size, 0.8..=2.0.
    pub fn interface_scale(&self) -> f32 {
        self.interface_scale
    }

    /// The rem size windows use: `16px * interface_scale`.
    pub fn rem_size(&self) -> Pixels {
        px(16. * self.interface_scale)
    }

    /// The palette for an explicit appearance, for scoped `.light` / `.dark`
    /// subtrees such as the gallery's side-by-side columns.
    pub fn colors_for(appearance: Appearance) -> Colors {
        match appearance {
            Appearance::Light => Colors::light(),
            Appearance::Dark => Colors::dark(),
        }
    }

    /// Changes the appearance preference and repaints every window.
    pub fn set_preference(preference: AppearancePreference, cx: &mut App) {
        Self::edit(cx, |theme| theme.preference = preference);
    }

    /// Sets the interface size, clamped to 0.8..=2.0 and rounded to 0.1.
    pub fn set_interface_scale(scale: f32, cx: &mut App) {
        let scale = ((scale / INTERFACE_SCALE_STEP).round() * INTERFACE_SCALE_STEP)
            .clamp(INTERFACE_SCALE_MIN, INTERFACE_SCALE_MAX);
        Self::edit(cx, |theme| theme.interface_scale = scale);
    }

    /// Mod+= in buzz-app.
    pub fn increase_interface_scale(cx: &mut App) {
        let scale = Self::base(cx).interface_scale + INTERFACE_SCALE_STEP;
        Self::set_interface_scale(scale, cx);
    }

    /// Mod+- in buzz-app.
    pub fn decrease_interface_scale(cx: &mut App) {
        let scale = Self::base(cx).interface_scale - INTERFACE_SCALE_STEP;
        Self::set_interface_scale(scale, cx);
    }

    /// Mod+0 in buzz-app.
    pub fn reset_interface_scale(cx: &mut App) {
        Self::set_interface_scale(1., cx);
    }

    /// Calls `f` after every change to the app theme's preference, system
    /// appearance or interface size, e.g. to save the settings.
    pub fn observe(cx: &mut App, mut f: impl FnMut(&mut App) + 'static) -> Subscription {
        cx.observe_global::<ThemeEdited>(move |cx| f(cx))
    }

    /// Records the system appearance reported by a window. While the
    /// preference is `System` this switches the palette.
    pub fn sync_system_appearance(window: &Window, cx: &mut App) {
        let system = Appearance::from(window.appearance());
        if Self::base(cx).system != system {
            Self::edit(cx, |theme| theme.system = system);
        }
    }

    fn edit(cx: &mut App, f: impl FnOnce(&mut Self)) {
        let theme = &mut cx.global_mut::<ThemeStack>().0[0];
        let before = (theme.appearance, theme.interface_scale);
        f(theme);
        theme.appearance = match theme.preference {
            AppearancePreference::System => theme.system,
            AppearancePreference::Light => Appearance::Light,
            AppearancePreference::Dark => Appearance::Dark,
        };
        theme.colors = Self::colors_for(theme.appearance);
        let after = (theme.appearance, theme.interface_scale);
        if before != after {
            crate::kit::sync(cx);
        }
        cx.set_global(ThemeEdited);
        cx.refresh_windows();
    }

    pub(crate) fn init(cx: &mut App) {
        let mut theme = Theme {
            system: cx.window_appearance().into(),
            ..Default::default()
        };
        theme.appearance = theme.system;
        theme.colors = Self::colors_for(theme.appearance);
        cx.set_global(ThemeStack(vec![theme]));
        crate::kit::sync(cx);
    }
}

/// `cx.theme()` access to the Buzz [`Theme`].
///
/// gpui-kit's components have a trait of the same shape; import only this
/// one in Buzz code.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;

    /// Shorthand for `theme().colors`.
    fn colors(&self) -> &Colors {
        &self.theme().colors
    }
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        Theme::global(self)
    }
}
