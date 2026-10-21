//! The window's own controls (`WindowControls.tsx`): Linux sessions get
//! no title bar from the compositor, so the header is the title bar and
//! ends with minimize, maximize and close. Windows windows have no caption
//! either, and get the same. On macOS the header is the title bar too,
//! but the system draws its window buttons over the header's left end.

use buzz_ui::{IconButton, IconButtonSize, IconName, Surface};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// Whether Buzz draws the window buttons; the system does on macOS.
const OWN_BUTTONS: bool = !cfg!(target_os = "macos");

/// Where macOS puts its window buttons (`trafficLightPosition`), and the
/// header's left padding that keeps clear of them (`.shell-header-mac`).
pub const MAC_BUTTONS: Point<Pixels> = point(px(20.), px(17.));
pub const MAC_BUTTONS_INSET: f32 = 80.;

/// Which controls the session gets (`window_controls.rs`): Hyprland has no
/// minimize, and stock Omarchy refuses maximize requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub minimize: bool,
    pub maximize: bool,
}

impl Policy {
    /// This session's controls; the environment doesn't change while the
    /// app runs.
    pub fn current() -> Self {
        static POLICY: std::sync::LazyLock<Policy> = std::sync::LazyLock::new(Policy::detect);
        *POLICY
    }

    fn detect() -> Self {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("XDG_SESSION_DESKTOP").ok())
            .unwrap_or_default();
        Self::for_session(
            cfg!(target_os = "linux"),
            &desktop,
            &std::env::var("OMARCHY_PATH").unwrap_or_default(),
        )
    }

    fn for_session(linux: bool, desktop: &str, omarchy_path: &str) -> Self {
        let hyprland = linux
            && desktop
                .split(':')
                .any(|name| name.trim().eq_ignore_ascii_case("Hyprland"));
        Self {
            minimize: !hyprland,
            maximize: !hyprland || omarchy_path.trim().is_empty(),
        }
    }
}

/// Minimize (or hide to the tray), maximize or restore, and close (or hide
/// to the tray), as the compositor allows.
pub fn render(policy: Policy, window: &Window) -> impl IntoElement {
    let supported = window.window_controls();
    let button = |id: &'static str, icon: IconName, label: &'static str| {
        IconButton::new(id, icon)
            .size(IconButtonSize::Sm)
            .surface(Surface::Header)
            .tooltip(label)
    };
    div()
        .flex()
        .items_center()
        .gap(rems(0.25))
        .when(
            OWN_BUTTONS && policy.minimize && supported.minimize,
            |this| {
                this.child(
                    button("window-minimize", IconName::Minus, "Minimize window")
                        .on_click(|_, window, cx| crate::main_window::minimize(window, cx)),
                )
            },
        )
        .when(
            OWN_BUTTONS && policy.maximize && supported.maximize,
            |this| {
                this.child(
                    button(
                        "window-maximize",
                        IconName::ArrowsOut,
                        "Maximize or restore window",
                    )
                    .on_click(|_, window, _| window.zoom_window()),
                )
            },
        )
        .when(OWN_BUTTONS, |this| {
            this.child(
                button("window-close", IconName::X, "Close window")
                    .on_click(|_, window, cx| crate::main_window::close(window, cx)),
            )
        })
}

/// Makes an element a title bar: drag to move, double-click to maximize
/// (as the desktop's settings say), right-click for the window menu.
/// Controls inside it must stop the mouse-down from reaching it
/// ([`not_title_bar`]). On Windows the system does all three, as for a
/// caption, once the area is marked for it.
pub fn title_bar(element: Stateful<Div>) -> Stateful<Div> {
    element
        .when(cfg!(windows), |this| {
            this.window_control_area(WindowControlArea::Drag)
        })
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
        .on_mouse_down(MouseButton::Right, |event, window, _| {
            if window.window_controls().window_menu {
                window.show_window_menu(event.position);
            }
        })
}

/// A part of the title bar that is not for dragging: buttons, tabs. On
/// Windows it hides the drag area from the system's hit test.
pub fn not_title_bar(element: Div) -> Div {
    element
        .when(cfg!(windows), |this| this.occlude())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn limits_controls_to_the_matching_session() {
        for (linux, desktop, path, minimize, maximize) in [
            (true, "Hyprland", "/usr/share/omarchy", false, false),
            (true, "vendor:hyprland", "/omarchy", false, false),
            (true, "Hyprland", "", false, true),
            (true, " Hyprland ", "  ", false, true),
            (true, "GNOME", "/omarchy", true, true),
            (true, "ubuntu:GNOME", "", true, true),
            (true, "KDE", "", true, true),
            (true, "NotHyprland", "/omarchy", true, true),
            (true, "", "/omarchy", true, true),
            (false, "Hyprland", "/omarchy", true, true),
        ] {
            assert_eq!(
                Policy::for_session(linux, desktop, path),
                Policy { minimize, maximize },
                "linux={linux}, desktop={desktop:?}, path={path:?}"
            );
        }
    }
}
