//! "Copy image" (buzz-app's `image_clipboard.rs`): one clipboard owned for
//! the app's lifetime, because on Linux a copied image is only available
//! while its owner runs. GPUI's own clipboard offers only text on Wayland,
//! so this goes through `arboard` as buzz-app does, on every platform. The
//! pixels come decoded from buzz-desktop's `media::copy_image`.

use std::sync::{Mutex, OnceLock};

/// Puts RGBA pixels on the clipboard. Blocking: run it off the UI thread.
pub fn write(width: usize, height: usize, rgba: Vec<u8>) -> Result<(), String> {
    static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    if width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Err("Could not copy image".into());
    }
    let mut stored = CLIPBOARD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "Clipboard unavailable".to_string())?;
    if stored.is_none() {
        *stored = Some(arboard::Clipboard::new().map_err(|_| "Clipboard unavailable".to_string())?);
    }
    stored
        .as_mut()
        .expect("clipboard initialized")
        .set_image(arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(rgba),
        })
        .map_err(|_| "Could not copy image".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn refuses_pixels_that_do_not_fill_the_image() {
        assert_eq!(write(2, 3, vec![0; 4]).unwrap_err(), "Could not copy image");
    }
}
