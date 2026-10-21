//! Emoji avatars (`paintEmojiAvatar`, `emojiAvatar`): the emoji drawn at
//! 258px from the bundled colour emoji font, trimmed to its painted pixels
//! and centred on a 512px square over the background colour, as PNG.
//! Also the colour maths of the custom colour picker
//! (`AvatarCustomColor.tsx`) and the gradients it paints.

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, fontdb,
};
use image::{ImageEncoder, RgbaImage};
use std::sync::{Arc, LazyLock, Mutex};

pub const SIZE: u32 = 512;
const FONT_SIZE: f32 = 258.;
/// Fainter pixels are fringe or shadow, not artwork.
const ALPHA_FLOOR: u8 = 32;

struct Fonts {
    system: FontSystem,
    cache: SwashCache,
}

/// Only the emoji font: emoji never fall back to a monochrome face.
static FONTS: LazyLock<Mutex<Fonts>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_font_source(fontdb::Source::Binary(Arc::new(buzz_ui::fonts::EMOJI_FONT)));
    Mutex::new(Fonts {
        system: FontSystem::new_with_locale_and_db("en-US".into(), db),
        cache: SwashCache::new(),
    })
});

/// The emoji alone, centred on a transparent 512px square; `None` when the
/// font paints nothing for it.
pub fn emoji_layer(emoji: &str) -> Option<RgbaImage> {
    let mut fonts = FONTS.lock().unwrap_or_else(|poison| poison.into_inner());
    let Fonts { system, cache } = &mut *fonts;
    let mut buffer = Buffer::new(system, Metrics::new(FONT_SIZE, FONT_SIZE * 1.5));
    buffer.set_size(None, None);
    buffer.set_text(
        emoji,
        &Attrs::new().family(Family::Name(buzz_ui::fonts::EMOJI)),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(system, false);
    // Glyph images and where they go, in drawing coordinates.
    let mut glyphs = Vec::new();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0., run.line_y), 1.);
            let Some(image) = cache.get_image_uncached(system, physical.cache_key) else {
                continue;
            };
            let x = physical.x + image.placement.left;
            let y = physical.y - image.placement.top;
            glyphs.push((x, y, image));
        }
    }
    let left = glyphs.iter().map(|(x, _, _)| *x).min()?;
    let top = glyphs.iter().map(|(_, y, _)| *y).min()?;
    let right = glyphs
        .iter()
        .map(|(x, _, image)| x + image.placement.width as i32)
        .max()?;
    let bottom = glyphs
        .iter()
        .map(|(_, y, image)| y + image.placement.height as i32)
        .max()?;
    let mut drawn = RgbaImage::new((right - left) as u32, (bottom - top) as u32);
    for (x, y, image) in &glyphs {
        let (width, height) = (image.placement.width, image.placement.height);
        for row in 0..height {
            for column in 0..width {
                let index = (row * width + column) as usize;
                let pixel = match image.content {
                    SwashContent::Color => {
                        let at = index * 4;
                        [
                            image.data[at],
                            image.data[at + 1],
                            image.data[at + 2],
                            image.data[at + 3],
                        ]
                    }
                    // A glyph without colour paints black, as the canvas does.
                    SwashContent::Mask => [0, 0, 0, image.data[index]],
                    SwashContent::SubpixelMask => continue,
                };
                let target =
                    drawn.get_pixel_mut((x - left) as u32 + column, (y - top) as u32 + row);
                target.0 = over(pixel, target.0);
            }
        }
    }
    Some(centered(&drawn))
}

/// Moves the painted pixels' box to the centre of a 512px square,
/// keeping the faint fringe around it.
fn centered(drawn: &RgbaImage) -> RgbaImage {
    let mut layer = RgbaImage::new(SIZE, SIZE);
    let painted = drawn
        .enumerate_pixels()
        .filter(|(_, _, pixel)| pixel.0[3] >= ALPHA_FLOOR);
    let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0, 0);
    let mut any = false;
    for (x, y, _) in painted {
        any = true;
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
    }
    if !any {
        return layer;
    }
    let half = (SIZE / 2) as f64;
    let dx = (half - (left + right + 1) as f64 / 2.).round() as i64;
    let dy = (half - (top + bottom + 1) as f64 / 2.).round() as i64;
    for (x, y, pixel) in drawn.enumerate_pixels() {
        let (tx, ty) = (x as i64 + dx, y as i64 + dy);
        if (0..SIZE as i64).contains(&tx) && (0..SIZE as i64).contains(&ty) {
            layer.put_pixel(tx as u32, ty as u32, *pixel);
        }
    }
    layer
}

/// `source` over `target`, straight alpha.
fn over(source: [u8; 4], target: [u8; 4]) -> [u8; 4] {
    let sa = source[3] as f32 / 255.;
    let ta = target[3] as f32 / 255.;
    let alpha = sa + ta * (1. - sa);
    if alpha <= 0. {
        return [0; 4];
    }
    let channel = |i: usize| {
        let value = (source[i] as f32 * sa + target[i] as f32 * ta * (1. - sa)) / alpha;
        value.round().clamp(0., 255.) as u8
    };
    [
        channel(0),
        channel(1),
        channel(2),
        (alpha * 255.).round() as u8,
    ]
}

/// The avatar file: the emoji over `color` (`#RRGGBB`), as PNG.
pub fn emoji_png(emoji: &str, color: &str) -> Result<Vec<u8>, String> {
    let invalid = || "Choose an emoji and a background color.".to_string();
    // JavaScript's length counts UTF-16 units.
    if emoji.is_empty() || emoji.encode_utf16().count() > 64 {
        return Err(invalid());
    }
    let background = parse_hex(color).ok_or_else(invalid)?;
    let layer = emoji_layer(emoji).ok_or("Could not prepare the emoji image.")?;
    let mut image = RgbaImage::from_pixel(
        SIZE,
        SIZE,
        image::Rgba([background[0], background[1], background[2], 255]),
    );
    for (target, source) in image.pixels_mut().zip(layer.pixels()) {
        target.0 = over(source.0, target.0);
    }
    encode_png(&image)
}

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| "Could not prepare the emoji image.".to_string())?;
    Ok(png)
}

/// `#RRGGBB`, any case.
pub fn parse_hex(color: &str) -> Option<[u8; 3]> {
    let digits = color.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// Hue in degrees, saturation and value in percent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsv {
    pub hue: f64,
    pub saturation: f64,
    pub value: f64,
}

/// `fromHex`.
pub fn hsv_from_hex(color: &str) -> Hsv {
    let [r, g, b] = parse_hex(color)
        .unwrap_or([0, 0, 0])
        .map(|c| c as f64 / 255.);
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    let hue = if delta == 0. {
        0.
    } else if max == r {
        (g - b) / delta
    } else if max == g {
        (b - r) / delta + 2.
    } else {
        (r - g) / delta + 4.
    };
    Hsv {
        hue: (hue * 60. + 360.) % 360.,
        saturation: if max > 0. { delta / max * 100. } else { 0. },
        value: max * 100.,
    }
}

/// `toHex`: uppercase `#RRGGBB`.
pub fn hex_from_hsv(hsv: Hsv) -> String {
    let (s, v) = (hsv.saturation / 100., hsv.value / 100.);
    let channel = |n: f64| {
        let k = (n + hsv.hue / 60.) % 6.;
        ((v - v * s * k.min(4. - k).clamp(0., 1.)) * 255.).round() as u8
    };
    format!("#{:02X}{:02X}{:02X}", channel(5.), channel(3.), channel(1.))
}

/// The colour at `t` (0 to 1) along evenly spaced `stops`.
fn along(stops: &[[u8; 3]], t: f64) -> [u8; 3] {
    let span = (stops.len() - 1) as f64;
    let at = (t.clamp(0., 1.) * span).min(span - 1e-9);
    let (index, mix) = (at.floor() as usize, at.fract());
    let (a, b) = (stops[index], stops[index + 1]);
    [0, 1, 2].map(|i| (a[i] as f64 + (b[i] as f64 - a[i] as f64) * mix).round() as u8)
}

/// BGRA pixels, as GPUI paints them.
fn bgra(image: &RgbaImage) -> Vec<u8> {
    let mut data = image.as_raw().clone();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    data
}

/// A GPUI image from straight RGBA pixels.
pub fn render_image(image: &RgbaImage) -> Option<Arc<gpui_kit::RenderImage>> {
    let buffer = RgbaImage::from_raw(image.width(), image.height(), bgra(image))?;
    Some(Arc::new(gpui_kit::RenderImage::new(vec![
        image::Frame::new(buffer),
    ])))
}

/// The custom colour button's conic gradient, starting at the top and
/// turning clockwise.
pub fn conic(size: u32) -> RgbaImage {
    const STOPS: [[u8; 3]; 6] = [
        [0xff, 0x4d, 0x4d],
        [0xff, 0xe7, 0x5c],
        [0x73, 0xef, 0x75],
        [0x63, 0xc6, 0xf2],
        [0xb1, 0x41, 0xff],
        [0xff, 0x4d, 0x4d],
    ];
    let centre = size as f64 / 2.;
    RgbaImage::from_fn(size, size, |x, y| {
        let (dx, dy) = (x as f64 + 0.5 - centre, y as f64 + 0.5 - centre);
        let turn = (dx.atan2(-dy) / std::f64::consts::TAU + 1.) % 1.;
        let [r, g, b] = along(&STOPS, turn);
        image::Rgba([r, g, b, 255])
    })
}

/// The hue track: red through the spectrum back to red, left to right.
pub fn hue_track(width: u32, height: u32) -> RgbaImage {
    const STOPS: [[u8; 3]; 7] = [
        [255, 0, 0],
        [255, 255, 0],
        [0, 255, 0],
        [0, 255, 255],
        [0, 0, 255],
        [255, 0, 255],
        [255, 0, 0],
    ];
    RgbaImage::from_fn(width, height, |x, _| {
        let [r, g, b] = along(&STOPS, (x as f64 + 0.5) / width as f64);
        image::Rgba([r, g, b, 255])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_conversions_match_the_reference() {
        for hex in [
            "#FFF4CC", "#476CFF", "#000000", "#FFFFFF", "#8A8F98", "#F6534F",
        ] {
            assert_eq!(hex_from_hsv(hsv_from_hex(hex)), hex);
        }
        let red = hsv_from_hex("#ff0000");
        assert_eq!((red.hue, red.saturation, red.value), (0., 100., 100.));
        let grey = hsv_from_hex("#808080");
        assert_eq!((grey.hue, grey.saturation), (0., 0.));
        assert_eq!(
            hex_from_hsv(Hsv {
                hue: 240.,
                saturation: 100.,
                value: 50.
            }),
            "#000080"
        );
        assert_eq!(parse_hex("#12abEF"), Some([0x12, 0xab, 0xef]));
        assert_eq!(parse_hex("12abef"), None);
        assert_eq!(parse_hex("#12abeg"), None);
    }

    #[test]
    fn emoji_is_drawn_centred_on_its_background() {
        let png = emoji_png("😀", "#FFF4CC").unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (SIZE, SIZE));
        // The corners are the background; the middle is emoji.
        assert_eq!(image.get_pixel(0, 0).0, [0xff, 0xf4, 0xcc, 255]);
        assert_ne!(image.get_pixel(256, 256).0, [0xff, 0xf4, 0xcc, 255]);
        // The painted box is centred within a pixel.
        let layer = emoji_layer("😀").unwrap();
        let painted: Vec<(u32, u32)> = layer
            .enumerate_pixels()
            .filter(|(_, _, p)| p.0[3] >= ALPHA_FLOOR)
            .map(|(x, y, _)| (x, y))
            .collect();
        let left = painted.iter().map(|p| p.0).min().unwrap();
        let right = painted.iter().map(|p| p.0).max().unwrap();
        let top = painted.iter().map(|p| p.1).min().unwrap();
        let bottom = painted.iter().map(|p| p.1).max().unwrap();
        assert!(((left + right + 1) as i64 - 512).abs() <= 1);
        assert!(((top + bottom + 1) as i64 - 512).abs() <= 1);
        // About the size the 258px canvas font draws.
        assert!((180..=320).contains(&(right - left)), "{}", right - left);
        // Sequences shape to one glyph.
        assert!(emoji_layer("👩‍🚀").is_some());
        assert!(emoji_png("", "#FFFFFF").is_err());
        assert!(emoji_png("😀", "red").is_err());
    }
}
