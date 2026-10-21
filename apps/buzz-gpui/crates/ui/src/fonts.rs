//! Bundled fonts: Inter (static 400/500/600/700, roman and italic),
//! JetBrains Mono 400 and 600 and Noto Color Emoji, all SIL OFL 1.1 (see
//! `assets/fonts/*/OFL.txt`).
//!
//! buzz-app loads Inter as a variable font. GPUI's Linux text stack picks
//! faces by weight from static fonts reliably, so the static instances of
//! the same Inter 4.1 release are bundled instead.
//!
//! Emoji: GPUI draws a glyph in colour only from a font whose PostScript
//! name is `NotoColorEmoji`, through swash, which reads its CBDT bitmaps
//! (swash has no COLRv1). Left to cosmic-text's own fallback, emoji Inter
//! lacks land in whichever system font covers them first, often DejaVu
//! Sans in monochrome, so Noto Color Emoji 2.051 is bundled and named as
//! every text style's fallback ([`fallbacks`]). Two edits make that work,
//! both done with fontTools:
//!
//! - GPUI drops any family without a glyph for `m` when it loads it by
//!   name, so the bundled copy maps U+006D to its space glyph (ASCII never
//!   falls back, so it is never drawn) and is renamed "Buzz Color Emoji",
//!   leaving any system Noto Color Emoji alone. Its PostScript name stays.
//! - GPUI keeps a character in the primary font whenever that font covers
//!   it, so the bundled Inter files have U+2600 ☀, U+2665 ♥, U+26A0 ⚠ and
//!   U+2764 ❤ removed from their `cmap` and draw as emoji the way browsers
//!   draw `❤️` and `⚠️`. Arrows, shapes and ©®™ keep Inter's text glyphs.

use gpui_kit::{App, FontFallbacks};
use std::borrow::Cow;

/// The colour emoji family.
pub const EMOJI: &str = "Buzz Color Emoji";

/// The colour emoji font itself, for drawing emoji outside GPUI (emoji
/// avatars).
pub const EMOJI_FONT: &[u8] =
    include_bytes!("../../../assets/fonts/buzz-color-emoji/BuzzColorEmoji.ttf");

const FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/inter/Inter-Regular.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Italic.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Medium.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-MediumItalic.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Bold.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-BoldItalic.ttf"),
    include_bytes!("../../../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    // The terminal's bold, as buzz-app's terminal loads 600 too.
    include_bytes!("../../../assets/fonts/jetbrains-mono/JetBrainsMono-SemiBold.ttf"),
    EMOJI_FONT,
];

/// Registers the bundled fonts with GPUI's text system.
pub fn register(cx: &mut App) {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!("buzz-ui: failed to register bundled fonts: {err:#}");
    }
}

/// The fallback chain for every text style: colour emoji before any
/// system font.
pub fn fallbacks() -> FontFallbacks {
    FontFallbacks::from_fonts(vec![EMOJI.to_string()])
}
