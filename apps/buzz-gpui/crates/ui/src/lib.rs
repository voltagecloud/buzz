//! Buzz design system for GPUI.
//!
//! A port of buzz-app's design system (`src/shared/design-system`): the
//! light and dark palettes, spacing, radii, type ramp and motion tokens,
//! the bundled Inter and JetBrains Mono fonts, the Tabler icon set, and
//! components styled to match buzz-app one to one. `docs/port/ui.md` is the
//! specification; section numbers in the docs here refer to it.
//!
//! ```no_run
//! use gpui_kit::*;
//!
//! gpui_kit::application().with_assets(buzz_ui::Assets).run(|cx| {
//!     gpui_kit::init(cx);
//!     buzz_ui::init(cx);
//! });
//! ```

mod assets;
mod background;
pub mod colors;
pub mod components;
pub mod fonts;
mod icon;
mod kit;
mod scope;
mod styled;
mod theme;
pub mod tokens;

pub use assets::Assets;
pub use background::ShellBackground;
pub use colors::Colors;
pub use components::*;
pub use icon::{Brand, BrandMark, Icon, IconName, Spinner};
pub use scope::ThemeScope;
pub use styled::{MaterialStyled, TypeStyled};
pub use theme::{
    ActiveTheme, Appearance, AppearancePreference, INTERFACE_SCALE_MAX, INTERFACE_SCALE_MIN,
    INTERFACE_SCALE_STEP, Interaction, Surface, Theme,
};
pub use tokens::TextRole;

use gpui_kit::{App, KeyBinding, TextRenderingMode, actions};

actions!(
    buzz_ui,
    [
        /// Mod+= : grow the interface by 10%.
        IncreaseInterfaceSize,
        /// Mod+- : shrink the interface by 10%.
        DecreaseInterfaceSize,
        /// Mod+0 : reset the interface size to 100%.
        ResetInterfaceSize,
        /// Escape in a [`Dialog`]: closes it. A dialog with a menu open
        /// closes the menu first by capturing this action.
        Cancel,
    ]
);

/// The key context of a [`Dialog`], where Escape is [`Cancel`].
pub const DIALOG_CONTEXT: &str = "BuzzDialog";

/// Initializes the design system. Call after `gpui_kit::init`.
///
/// Registers the bundled fonts, installs the [`Theme`] global (following
/// the system appearance), projects it onto gpui-kit's theme, binds the
/// interface size shortcuts (an app may rebind them) and keeps every
/// window's appearance in sync.
pub fn init(cx: &mut App) {
    fonts::register(cx);
    // buzz-app sets `-webkit-font-smoothing: antialiased`: grayscale AA.
    cx.set_text_rendering_mode(TextRenderingMode::Grayscale);
    Theme::init(cx);

    cx.on_action(|_: &IncreaseInterfaceSize, cx| Theme::increase_interface_scale(cx));
    cx.on_action(|_: &DecreaseInterfaceSize, cx| Theme::decrease_interface_scale(cx));
    cx.on_action(|_: &ResetInterfaceSize, cx| Theme::reset_interface_scale(cx));
    cx.bind_keys([
        KeyBinding::new("secondary-=", IncreaseInterfaceSize, None),
        KeyBinding::new("secondary-+", IncreaseInterfaceSize, None),
        KeyBinding::new("secondary-shift-=", IncreaseInterfaceSize, None),
        KeyBinding::new("secondary-shift-+", IncreaseInterfaceSize, None),
        KeyBinding::new("secondary--", DecreaseInterfaceSize, None),
        KeyBinding::new("secondary-0", ResetInterfaceSize, None),
        KeyBinding::new("escape", Cancel, Some(DIALOG_CONTEXT)),
    ]);

    // Every window gets a gpui-kit Root; follow its system appearance.
    cx.observe_new(|_: &mut gpui_kit::base::Root, window, cx| {
        if let Some(window) = window {
            Theme::sync_system_appearance(window, cx);
            window
                .observe_window_appearance(|window, cx| Theme::sync_system_appearance(window, cx))
                .detach();
        }
    })
    .detach();
}
