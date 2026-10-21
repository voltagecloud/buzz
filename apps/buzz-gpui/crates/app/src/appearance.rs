//! Keeps the appearance settings across launches: the color mode
//! (`buzz-appearance.v1`) and the interface size (`buzz-font-scale.v1`),
//! as device preferences in local storage. A setting that cannot be
//! restored or saved stays in effect and is reported to Settings →
//! Appearance, as buzz-app's theme service does.

use crate::community::AppState;
use buzz_ui::{AppearancePreference, Theme};
use gpui_kit::{App, Global};

const APPEARANCE: &str = "appearance";
const INTERFACE_SCALE: &str = "interface-scale";

const APPEARANCE_RESTORE: &str =
    "Appearance could not be restored. Choose a mode to try saving it again.";
const APPEARANCE_SAVE: &str =
    "This appearance is active, but could not be saved on this device. Try again.";
const SCALE_RESTORE: &str =
    "Interface size could not be restored. Choose a size to try saving it again.";
const SCALE_SAVE: &str =
    "This interface size is active, but could not be saved on this device. Try again.";

/// What could not be restored or saved, for Settings' toasts.
#[derive(Default)]
pub struct AppearanceErrors {
    pub appearance: Option<&'static str>,
    pub scale: Option<&'static str>,
}

impl Global for AppearanceErrors {}

/// Applies the saved settings, then saves every later change, whether it
/// comes from Settings or the Mod+= / Mod+- / Mod+0 shortcuts.
pub fn init(cx: &mut App) {
    let store = cx.global::<AppState>().store.clone();
    let mut errors = AppearanceErrors::default();
    match store.device_pref(APPEARANCE) {
        Ok(saved) => {
            if let Some(preference) = saved.as_deref().and_then(parse) {
                Theme::set_preference(preference, cx);
            }
        }
        Err(_) => errors.appearance = Some(APPEARANCE_RESTORE),
    }
    match store.device_pref(INTERFACE_SCALE) {
        Ok(saved) => {
            if let Some(scale) = saved.and_then(|value| value.parse::<f32>().ok()) {
                Theme::set_interface_scale(scale, cx);
            }
        }
        Err(_) => errors.scale = Some(SCALE_RESTORE),
    }
    cx.set_global(errors);
    let mut last = current(cx);
    Theme::observe(cx, move |cx| {
        let now = current(cx);
        if now == last {
            return;
        }
        if now.0 != last.0 {
            save_appearance(cx);
        }
        if now.1 != last.1 {
            save_scale(cx);
        }
        last = now;
    })
    .detach();
}

/// "Retry saving appearance".
pub fn save_appearance(cx: &mut App) {
    let preference = current(cx).0;
    let saved = cx
        .global::<AppState>()
        .store
        .set_device_pref(APPEARANCE, &format!("\"{}\"", name(preference)));
    if let Err(error) = &saved {
        tracing::warn!(%error, "could not save the appearance");
    }
    cx.global_mut::<AppearanceErrors>().appearance = saved.err().map(|_| APPEARANCE_SAVE);
    cx.refresh_windows();
}

/// "Retry saving interface size".
pub fn save_scale(cx: &mut App) {
    let scale = current(cx).1;
    let saved = cx
        .global::<AppState>()
        .store
        .set_device_pref(INTERFACE_SCALE, &format!("{scale:.1}"));
    if let Err(error) = &saved {
        tracing::warn!(%error, "could not save the interface size");
    }
    cx.global_mut::<AppearanceErrors>().scale = saved.err().map(|_| SCALE_SAVE);
    cx.refresh_windows();
}

fn current(cx: &App) -> (AppearancePreference, f32) {
    let theme = Theme::base(cx);
    (theme.preference(), theme.interface_scale())
}

fn name(preference: AppearancePreference) -> &'static str {
    match preference {
        AppearancePreference::System => "system",
        AppearancePreference::Light => "light",
        AppearancePreference::Dark => "dark",
    }
}

fn parse(json: &str) -> Option<AppearancePreference> {
    match json.trim_matches('"') {
        "system" => Some(AppearancePreference::System),
        "light" => Some(AppearancePreference::Light),
        "dark" => Some(AppearancePreference::Dark),
        _ => None,
    }
}
