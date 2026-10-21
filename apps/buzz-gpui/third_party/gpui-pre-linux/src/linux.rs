mod dispatcher;
mod display_connection;
mod headless;
mod keyboard;
mod platform;
mod system_notifications;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod text_system;
#[cfg(feature = "wayland")]
mod wayland;
#[cfg(feature = "x11")]
mod x11;

#[cfg(any(feature = "wayland", feature = "x11"))]
mod xdg_desktop_portal;

pub use dispatcher::*;
pub(crate) use display_connection::{Backend, DisplayConnection, select_backend};
pub(crate) use headless::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) use text_system::*;
#[cfg(feature = "wayland")]
pub(crate) use wayland::*;
#[cfg(feature = "x11")]
pub(crate) use x11::*;

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{ClipboardEntry, ClipboardItem, ClipboardString, ExternalPaths, WindowingModes};

/// Returns the default platform implementation for the current OS.
///
/// A windowed platform connects to the display server the process environment names, or starts
/// headless when it names none, and can switch between those modes later. A headless platform
/// stays headless.
pub fn current_platform(headless: bool) -> Rc<dyn gpui::Platform> {
    if headless || std::env::var_os("ZED_HEADLESS").is_some() {
        linux_platform(WindowingModes::HEADLESS)
    } else {
        linux_platform(WindowingModes::all())
    }
}

/// Returns a platform that may switch among `allowed_modes`.
///
/// It starts windowed in the process's own environment, or headless if that names no allowed
/// display server. Set another initial mode with [`gpui::Application::with_windowing`].
pub fn linux_platform(allowed_modes: WindowingModes) -> Rc<dyn gpui::Platform> {
    Rc::new(LinuxPlatform::new(allowed_modes))
}

thread_local! {
    static ACTIVATION_TOKEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Raises the next window to be activated with `token`, an xdg-activation
/// token handed to the app: by a notification server when the user clicks a
/// notification, or by another process of the app that the user launched.
///
/// Wayland compositors refuse to raise a window that asks on its own while
/// another app has focus, so call this just before
/// [`gpui::Window::activate_window`]. The token is used once. X11 doesn't
/// need it and drops it.
pub fn set_activation_token(token: String) {
    ACTIVATION_TOKEN.with(|slot| *slot.borrow_mut() = Some(token));
}

#[cfg_attr(not(any(feature = "wayland", feature = "x11")), allow(dead_code))]
pub(crate) fn take_activation_token() -> Option<String> {
    ACTIVATION_TOKEN.with(|slot| slot.borrow_mut().take())
}

/// Files copied in a file manager, from a `text/uri-list` clipboard offer:
/// the paths, then the same paths as text for fields that only take text.
/// `None` when the list names no local files.
#[cfg_attr(not(any(feature = "wayland", feature = "x11")), allow(dead_code))]
pub(crate) fn file_list_clipboard_item(uri_list: &str) -> Option<ClipboardItem> {
    let paths: smallvec::SmallVec<[std::path::PathBuf; 2]> = uri_list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| url::Url::parse(line).ok())
        .filter(|url| url.scheme() == "file")
        .filter_map(|url| url.to_file_path().ok())
        .collect();
    if paths.is_empty() {
        return None;
    }
    let text = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    Some(ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(ExternalPaths(paths)),
            ClipboardEntry::String(ClipboardString::new(text)),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_lists_become_paths_and_text() {
        let item = file_list_clipboard_item(
            "# copied\r\nfile:///tmp/a%20b.png\r\nhttps://example.com/x\r\nfile:///tmp/c\r\n",
        )
        .unwrap();
        let ClipboardEntry::ExternalPaths(paths) = &item.entries[0] else {
            panic!("paths first");
        };
        assert_eq!(
            paths.paths(),
            [
                std::path::PathBuf::from("/tmp/a b.png"),
                std::path::PathBuf::from("/tmp/c")
            ]
        );
        assert_eq!(item.text().as_deref(), Some("/tmp/a b.png\n/tmp/c"));
        assert!(file_list_clipboard_item("https://example.com/x\n").is_none());
    }

    #[test]
    fn activation_tokens_are_used_once() {
        set_activation_token("token".into());
        assert_eq!(take_activation_token().as_deref(), Some("token"));
        assert_eq!(take_activation_token(), None);
    }
}
