//! The tray icon, with Open Buzz and Quit Buzz, which carries a dot while
//! messages are unread: a StatusNotifierItem on
//! Linux ([`linux`]), a menu-bar item on macOS and a notification-area
//! icon on Windows ([`native`]).

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;
#[cfg(any(target_os = "macos", windows))]
mod native;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use linux::Tray;
#[cfg(any(target_os = "macos", windows))]
pub use native::Tray;

/// What the tray asks of the app.
#[derive(Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Whether a panel is showing the icon.
    Shown(bool),
    Quit,
}

/// What is unread, as the icon shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    /// Messages the rail marks as unread: the dot.
    pub any: bool,
    /// Unread mentions and direct messages, which the tooltip counts.
    pub attention: u32,
}

impl Unread {
    fn dot(self) -> bool {
        self.any || self.attention > 0
    }
}

fn tool_tip(unread: Unread) -> String {
    match unread.attention {
        0 if unread.any => "Unread messages".into(),
        0 => String::new(),
        1 => "1 unread mention or direct message".into(),
        n => format!("{n} unread mentions and direct messages"),
    }
}

const SOURCES: [&[u8]; 2] = [
    include_bytes!("../../../../assets/icons/buzz-128.png"),
    include_bytes!("../../../../assets/icons/buzz-256.png"),
];

/// The app icon at `size` pixels, with the attention dot if `dot`.
fn icon(size: u32, dot: bool) -> Option<image::RgbaImage> {
    let source = image::load_from_memory(SOURCES[1])
        .or_else(|_| image::load_from_memory(SOURCES[0]))
        .ok()?;
    let mut icon = source
        .resize_exact(size, size, image::imageops::FilterType::Triangle)
        .to_rgba8();
    if dot {
        draw_dot(&mut icon);
    }
    Some(icon)
}

/// A red dot with a white ring in the top-right corner.
fn draw_dot(icon: &mut image::RgbaImage) {
    let size = icon.width() as f32;
    let radius = size * 0.22;
    let ring = (size * 0.06).max(1.);
    let (cx, cy) = (size - radius - 0.5, radius + 0.5);
    for (x, y, pixel) in icon.enumerate_pixels_mut() {
        let distance = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
        if distance <= radius - ring {
            pixel.0 = [0xe5, 0x48, 0x4d, 0xff];
        } else if distance <= radius {
            pixel.0 = [0xff, 0xff, 0xff, 0xff];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_tips_count_what_is_unread() {
        let unread = |any, attention| Unread { any, attention };
        assert_eq!(tool_tip(unread(false, 0)), "");
        assert_eq!(tool_tip(unread(true, 0)), "Unread messages");
        assert_eq!(
            tool_tip(unread(true, 2)),
            "2 unread mentions and direct messages"
        );
    }

    #[test]
    fn the_dot_marks_the_icon() {
        let plain = icon(32, false).unwrap();
        let marked = icon(32, true).unwrap();
        assert_eq!(plain.dimensions(), (32, 32));
        assert_ne!(plain, marked);
    }
}
