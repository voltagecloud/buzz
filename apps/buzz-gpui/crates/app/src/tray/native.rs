//! The tray icon on macOS (a menu-bar item) and Windows (a
//! notification-area icon), through tray-icon, the crate behind Tauri's
//! tray. It lives on the main thread, whose event loop GPUI runs.
//!
//! A click on the Windows icon brings the window back, and a right-click
//! opens the menu; a click on a menu-bar item opens its menu, as they all
//! do on macOS. While mentions or direct messages are unread the icon
//! carries a red dot, and its tooltip counts them.

use super::{TrayEvent, icon, tool_tip};
use crate::ui::navigation::{Arrival, Request};
use futures::channel::mpsc::UnboundedSender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// The image's pixels; the system scales it to the bar or the tray.
const SIZE: u32 = if cfg!(target_os = "macos") { 44 } else { 32 };

/// The tray icon while it is up; dropping it takes the icon down.
pub struct Tray {
    icon: TrayIcon,
    attention: u32,
}

impl Tray {
    /// Puts the icon up. Requests to open go to navigation; the rest come
    /// back as [`TrayEvent`]s. Call on the main thread.
    pub fn start(
        navigation: UnboundedSender<Arrival>,
        events: UnboundedSender<TrayEvent>,
    ) -> Result<Self, String> {
        let open = MenuItem::new("Open Buzz", true, None);
        let quit = MenuItem::new("Quit Buzz", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])
            .map_err(|error| error.to_string())?;
        let focus = move || {
            let _ = navigation.unbounded_send(Request::Focus.into());
        };
        let (open, quit) = (open.id().clone(), quit.id().clone());
        let clicked = focus.clone();
        let asked = events.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open {
                clicked();
            } else if event.id == quit {
                let _ = asked.unbounded_send(TrayEvent::Quit);
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if cfg!(windows)
                && let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
            {
                focus();
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_id("buzz")
            .with_tooltip("Buzz")
            .with_icon(image(false)?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()
            .map_err(|error| error.to_string())?;
        // The system always shows it, if only in the overflow.
        let _ = events.unbounded_send(TrayEvent::Shown(true));
        Ok(Self { icon, attention: 0 })
    }

    /// Asks for attention while `count` mentions and direct messages are
    /// unread.
    pub fn set_attention(&mut self, count: u32) -> Result<(), String> {
        if self.attention == count {
            return Ok(());
        }
        if (self.attention > 0) != (count > 0) {
            self.icon
                .set_icon(Some(image(count > 0)?))
                .map_err(|error| error.to_string())?;
        }
        self.attention = count;
        let tip = match tool_tip(count) {
            tip if tip.is_empty() => "Buzz".to_string(),
            tip => format!("Buzz: {tip}"),
        };
        self.icon
            .set_tooltip(Some(tip))
            .map_err(|error| error.to_string())
    }
}

impl Drop for Tray {
    /// The handlers hold the senders; dropping them ends the app's wait
    /// for this icon's events.
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
    }
}

fn image(dot: bool) -> Result<Icon, String> {
    let icon = icon(SIZE, dot).ok_or("Could not draw the tray icon")?;
    Icon::from_rgba(icon.into_raw(), SIZE, SIZE).map_err(|error| error.to_string())
}
