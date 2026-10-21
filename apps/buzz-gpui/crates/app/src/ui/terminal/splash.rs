//! The terminal's welcome splash (`splash.ts`, `banner.ts`): the "buzz"
//! wordmark in compact three-cell strokes on a staggered hexagon field,
//! drifting and rippling toward the pointer for three seconds after the
//! shell's first output. Decoration only.

use buzz_ui::Colors;
use gpui_kit::{Bounds, Hsla, PathBuilder, Pixels, Rgba, Window, point, px};
use std::collections::HashMap;
use std::f32::consts::PI;
use std::time::{Duration, Instant};

/// How long the splash stays before it dismisses itself.
pub const DURATION: Duration = Duration::from_secs(3);
/// A click's ripple lasts this long, in milliseconds.
const PULSE_MS: f32 = 1200.;

/// `--art-terminal-start` and `--art-terminal-end` (`--orange-9`,
/// `--amber-9`), the same in light and dark.
const ART_START: u32 = 0xf76b15;
const ART_END: u32 = 0xffc53d;

/// 12 rows by 38 columns: b, u, z and z, eight columns each with two
/// between letters.
fn wordmark() -> Vec<Vec<bool>> {
    (0..12)
        .map(|y: i32| {
            let mut row = Vec::with_capacity(38);
            for (index, letter) in "buzz".chars().enumerate() {
                for x in 0..8 {
                    let ink = if y < 3 {
                        letter == 'b' && x < 3
                    } else {
                        let bar = !(5..10).contains(&y);
                        match letter {
                            'b' => !(3..5).contains(&x) || bar,
                            'u' => !(3..5).contains(&x) || y >= 10,
                            _ => {
                                let diagonal = (5. * (1. - (y - 3) as f32 / 8.)).round() as i32;
                                bar || (x >= diagonal && x < diagonal + 3)
                            }
                        }
                    };
                    row.push(ink);
                }
                if index < 3 {
                    row.extend([false, false]);
                }
            }
            row
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct BannerCell {
    pub x: usize,
    pub y: usize,
    pub cx: f32,
    pub cy: f32,
    pub ink: bool,
    pub field: f32,
    pub seed: f32,
    pub edge: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Banner {
    pub width: f32,
    pub height: f32,
    pub cell: f32,
    pub cells: Vec<BannerCell>,
}

/// The hexagon field for a `width` × `height` area, or `None` when it is
/// too small to hold the wordmark.
pub fn build(width: f32, height: f32) -> Option<Banner> {
    let sqrt3 = 3f32.sqrt();
    let cell = 12f32
        .min((width - 24.) / 40.)
        .min((height - 24.) / (16. * sqrt3 / 2.));
    if cell.is_nan() || cell < 1. {
        return None;
    }
    let row_height = cell * sqrt3 / 2.;
    let columns = 160.min(((width - 24.) / cell).floor() as i32);
    let rows = 80.min(((height - 24.) / row_height).floor() as i32);
    let left = (columns - 38).div_euclid(2);
    let top = (rows - 12).div_euclid(2);
    let ox = (width - columns as f32 * cell) / 2.;
    let oy = (height - rows as f32 * row_height) / 2.;
    let mark = wordmark();
    let cells = (0..columns * rows)
        .map(|index| {
            let (x, y) = (index % columns, index / columns);
            let (fx, fy) = (x as f32, y as f32);
            let ink = usize::try_from(y - top)
                .ok()
                .zip(usize::try_from(x - left).ok())
                .and_then(|(row, column)| mark.get(row)?.get(column).copied())
                .unwrap_or(false);
            // In f64, as JavaScript computes it.
            let seed = (((x as f64 * 127.1 + y as f64 * 311.7).sin() * 43_758.545_3) % 1.) as f32;
            BannerCell {
                x: x as usize,
                y: y as usize,
                cx: ox + (fx + 0.25 + (y % 2) as f32 * 0.5) * cell,
                cy: oy + (fy + 0.5) * row_height,
                ink,
                field: (fx * 0.13 + fy * 0.085).sin() * 1.4
                    + (fy * 0.24 - fx * 0.07).cos()
                    + (fx * 0.055 - fy * 0.19).sin() * 0.7,
                seed: seed.abs(),
                edge: ((fx
                    .min((columns - 1 - x) as f32)
                    .min(fy * 1.4)
                    .min((rows - 1 - y) as f32 * 1.4)
                    + 1.)
                    / 4.)
                    .min(1.),
            }
        })
        .collect();
    Some(Banner {
        width,
        height,
        cell,
        cells,
    })
}

/// sRGB channels, unclamped so a ramp between out-of-gamut ends mixes the
/// way `color-mix(in srgb, …)` does before the canvas clamps it.
type Srgb = [f32; 3];

fn hex(value: u32) -> Srgb {
    [
        (value >> 16 & 0xff) as f32 / 255.,
        (value >> 8 & 0xff) as f32 / 255.,
        (value & 0xff) as f32 / 255.,
    ]
}

fn to_linear(c: f32) -> f32 {
    if c.abs() <= 0.04045 {
        c / 12.92
    } else {
        c.signum() * ((c.abs() + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(c: f32) -> f32 {
    if c.abs() <= 0.0031308 {
        c * 12.92
    } else {
        c.signum() * (1.055 * c.abs().powf(1. / 2.4) - 0.055)
    }
}

/// sRGB to OKLCH (lightness, chroma, hue in degrees).
fn oklch(color: Srgb) -> [f32; 3] {
    let [r, g, b] = color.map(to_linear);
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    let lightness = 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s;
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let bb = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    let hue = bb.atan2(a).to_degrees().rem_euclid(360.);
    [lightness, (a * a + bb * bb).sqrt(), hue]
}

fn from_oklch([lightness, chroma, hue]: [f32; 3]) -> Srgb {
    let (a, b) = (
        chroma * hue.to_radians().cos(),
        chroma * hue.to_radians().sin(),
    );
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
    .map(from_linear)
}

/// `oklch(from <color> calc(l + 0.02) calc(c * chroma) calc(h + hue))`.
fn relative(color: u32, chroma: f32, hue: f32) -> Srgb {
    let [l, c, h] = oklch(hex(color));
    from_oklch([l + 0.02, c * chroma, h + hue])
}

fn srgb(color: Hsla) -> Srgb {
    let color = Rgba::from(color);
    [color.r, color.g, color.b]
}

/// 64 steps from `start` to `end`, clamped as a canvas reads them back.
fn ramp(start: Srgb, end: Srgb) -> Vec<Rgba> {
    (0..64)
        .map(|index| {
            let t = index as f32 / 63.;
            let mix =
                |i: usize| ((start[i] + (end[i] - start[i]) * t).clamp(0., 1.) * 255.).round();
            Rgba {
                r: mix(0) / 255.,
                g: mix(1) / 255.,
                b: mix(2) / 255.,
                a: 1.,
            }
        })
        .collect()
}

/// The splash's resolved colours (`bannerColors` with the `.splash`
/// custom properties).
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub surface: Hsla,
    pub ink: Vec<Rgba>,
    pub field: Vec<Rgba>,
    pub opacity: f32,
}

impl Palette {
    pub fn new(colors: &Colors, dark: bool) -> Self {
        let (chroma, hue, opacity) = if dark {
            (1.21, 1., 0.25)
        } else {
            (1.29, 0., 0.15)
        };
        Self {
            surface: colors.surface_panel,
            ink: ramp(
                relative(ART_START, chroma, hue),
                relative(ART_END, chroma, hue),
            ),
            field: ramp(srgb(colors.border_standard), srgb(colors.border_prominent)),
            opacity,
        }
    }
}

/// Where the pointer is, as a fraction of the splash, and how strongly
/// the field follows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub x: f32,
    pub y: f32,
    pub target_x: f32,
    pub target_y: f32,
    pub strength: f32,
    pub inside: bool,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            x: 0.5,
            y: 0.5,
            target_x: 0.5,
            target_y: 0.5,
            strength: 0.,
            inside: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pulse {
    pub x: f32,
    pub y: f32,
    /// Elapsed milliseconds when it was clicked.
    pub at: f32,
}

/// One showing of the splash: its clock, the pointer it follows and the
/// ripples clicked into it.
pub struct Splash {
    pub elapsed: f32,
    previous: Option<Instant>,
    pub pointer: Pointer,
    pub pulses: Vec<Pulse>,
}

impl Splash {
    pub fn new() -> Self {
        Self {
            elapsed: 0.,
            previous: None,
            pointer: Pointer::default(),
            pulses: Vec::new(),
        }
    }

    /// Advances the animation one frame (`tick`).
    pub fn tick(&mut self, now: Instant) {
        let dt = self
            .previous
            .map(|previous| (now - previous).as_secs_f32() * 1000.)
            .unwrap_or(0.)
            .min(80.);
        self.previous = Some(now);
        self.elapsed += dt;
        let follow = 1. - (-dt / 160.).exp();
        let pointer = &mut self.pointer;
        pointer.x += (pointer.target_x - pointer.x) * follow;
        pointer.y += (pointer.target_y - pointer.y) * follow;
        let goal = if pointer.inside { 1. } else { 0. };
        pointer.strength += (goal - pointer.strength) * follow;
        let elapsed = self.elapsed;
        self.pulses.retain(|pulse| elapsed - pulse.at < PULSE_MS);
    }

    /// The animation stopped and starts again from rest (`resume`).
    pub fn rest(&mut self) {
        self.previous = None;
        self.pointer.inside = false;
        self.pointer.strength = 0.;
        self.pulses.clear();
    }

    pub fn hover(&mut self, x: f32, y: f32) {
        let pointer = &mut self.pointer;
        pointer.target_x = x;
        pointer.target_y = y;
        if !pointer.inside && pointer.strength < 0.01 {
            pointer.x = x;
            pointer.y = y;
        }
        pointer.inside = true;
    }

    pub fn leave(&mut self) {
        self.pointer.inside = false;
    }

    /// Keeps the two newest ripples and adds one.
    pub fn click(&mut self, x: f32, y: f32) {
        let keep = self.pulses.len().saturating_sub(2);
        self.pulses.drain(..keep);
        self.pulses.push(Pulse {
            x,
            y,
            at: self.elapsed,
        });
    }
}

impl Default for Splash {
    fn default() -> Self {
        Self::new()
    }
}

/// Clips the segment `a`–`b` to a convex polygon (Cyrus–Beck).
fn clip(a: (f32, f32), b: (f32, f32), polygon: &[(f32, f32)]) -> Option<((f32, f32), (f32, f32))> {
    let (mut enter, mut leave) = (0f32, 1f32);
    let direction = (b.0 - a.0, b.1 - a.1);
    let n = polygon.len();
    // The hexagon winds clockwise in screen space, so inward normals are
    // to the right of each edge.
    for i in 0..n {
        let (p, q) = (polygon[i], polygon[(i + 1) % n]);
        let edge = (q.0 - p.0, q.1 - p.1);
        let normal = (-edge.1, edge.0);
        let numerator = normal.0 * (a.0 - p.0) + normal.1 * (a.1 - p.1);
        let denominator = normal.0 * direction.0 + normal.1 * direction.1;
        if denominator.abs() < f32::EPSILON {
            if numerator < 0. {
                return None;
            }
            continue;
        }
        let t = -numerator / denominator;
        if denominator > 0. {
            enter = enter.max(t);
        } else {
            leave = leave.min(t);
        }
        if enter > leave {
            return None;
        }
    }
    Some((
        (a.0 + direction.0 * enter, a.1 + direction.1 * enter),
        (a.0 + direction.0 * leave, a.1 + direction.1 * leave),
    ))
}

/// buzz-app dots unhatched outlines (`setLineDash([1, 1.6])`). Dashing
/// thousands of tiny hexagons every frame is the costliest part of the
/// splash here, so they are drawn solid with the dots' share of ink.
const DOTTED_INK: f32 = 1. / 2.6;

/// Strokes that share a colour, alpha and width, painted as one path.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Stroke {
    ink: bool,
    shade: u8,
    alpha: u8,
    width: u8,
}

/// Draws one frame of the banner into `bounds` (`drawTerminalBanner`).
pub fn paint(
    bounds: Bounds<Pixels>,
    banner: &Banner,
    palette: &Palette,
    splash: &Splash,
    window: &mut Window,
) {
    window.paint_quad(gpui_kit::fill(bounds, palette.surface));
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let Banner {
        width,
        height,
        cell,
        ..
    } = *banner;
    let time = splash.elapsed / 1000.;
    let pointer = splash.pointer;
    let mut strokes: HashMap<Stroke, Vec<Vec<(f32, f32)>>> = HashMap::new();
    for p in &banner.cells {
        let distance =
            ((p.cx - pointer.x * width) / cell).hypot((p.cy - pointer.y * height) / cell);
        let mut wave = 0.;
        for pulse in &splash.pulses {
            let age = (splash.elapsed - pulse.at) / PULSE_MS;
            let radius = ((p.cx - pulse.x * width) / cell).hypot((p.cy - pulse.y * height) / cell);
            let ring = (radius - age * 30.) / 3.;
            wave += ring.cos() * (-ring * ring * 0.5).exp() * (PI * age).sin() * (1. - age) * 0.3;
        }
        let ripple =
            (distance * 0.65 - time * 3.).sin() * (-distance / 18.).exp() * pointer.strength * 0.65
                + wave.clamp(-0.2, 0.2);
        let field = p.field + (p.y as f32 * 0.17 + time * 0.36).sin() * 0.55 + ripple * 2.4;
        let phase = field * 1.25 + time * 0.22 + 4.5;
        let blend = phase.rem_euclid(2.);
        let ramp_length = if p.ink {
            palette.ink.len()
        } else {
            palette.field.len()
        };
        let shade = ((if blend > 1. { 2. - blend } else { blend }) * (ramp_length - 1) as f32)
            .round() as u8;
        let neutral = ((0.3 + 0.38 * (field + 3.6) / 7.2 + ripple.max(0.) * 0.22) / 0.65).min(1.)
            * palette.opacity;
        let alpha = (if p.ink {
            0.92 * (1. + ripple * 0.35)
        } else {
            neutral
        } * p.edge)
            .clamp(0., 1.);
        if alpha <= 0.004 {
            continue;
        }
        let line_width = 0.8 + ripple.max(0.) * 0.65;
        // A quarter-cell gap between neighbouring hexagon faces.
        let size = (cell * 0.75 * 2. / 3f32.sqrt()) * (1. + ripple * 0.12);
        let (x, y) = (p.cx - size / 2., p.cy - size / 2.);
        let hatch = (field * 2. + time * 0.4 + p.seed * 0.45).sin() > 0.3;
        let corners: Vec<(f32, f32)> = (0..6)
            .map(|corner| {
                let angle = corner as f32 * PI / 3. - PI / 2.;
                (
                    p.cx + angle.cos() * size / 2.,
                    p.cy + angle.sin() * size / 2.,
                )
            })
            .collect();
        let ink = if hatch { alpha } else { alpha * DOTTED_INK };
        let key = Stroke {
            ink: p.ink,
            shade,
            alpha: (ink * 40.).round() as u8,
            width: (line_width * 10.).round() as u8,
        };
        if key.alpha == 0 {
            continue;
        }
        let paths = strokes.entry(key).or_default();
        let mut outline = corners.clone();
        outline.push(corners[0]);
        paths.push(outline);
        if hatch {
            let mut d = 3.;
            while d < size * 2. {
                let a = (x + (d - size).max(0.), y + d.min(size));
                let b = (x + d.min(size), y + (d - size).max(0.));
                if let Some((a, b)) = clip(a, b, &corners) {
                    paths.push(vec![a, b]);
                }
                d += 3.;
            }
        }
    }
    for (key, paths) in strokes {
        let ramp = if key.ink {
            &palette.ink
        } else {
            &palette.field
        };
        let Some(color) = ramp.get(key.shade as usize) else {
            continue;
        };
        let mut builder = PathBuilder::stroke(px(key.width as f32 / 10.));
        for path in paths {
            let mut points = path.into_iter();
            let Some(first) = points.next() else {
                continue;
            };
            builder.move_to(point(px(origin.0 + first.0), px(origin.1 + first.1)));
            for next in points {
                builder.line_to(point(px(origin.0 + next.0), px(origin.1 + next.1)));
            }
        }
        if let Ok(path) = builder.build() {
            let color = Rgba {
                a: key.alpha as f32 / 40.,
                ..*color
            };
            window.paint_path(path, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn fits_the_complete_wordmark() {
        for (width, height) in [(1200., 560.), (360., 180.), (280., 78.)] {
            let banner = build(width, height).expect("a banner");
            let ink: Vec<_> = banner.cells.iter().filter(|cell| cell.ink).collect();
            // b + u + z + z, no clipped cells.
            assert_eq!(ink.len(), 71 + 58 + 47 * 2, "{width}x{height}");
            for cell in ink {
                assert!(cell.cx > 0. && cell.cx < width);
                assert!(cell.cy > 0. && cell.cy < height);
            }
        }
    }

    #[test]
    fn no_field_for_a_zero_sized_host() {
        assert_eq!(build(0., 0.), None);
    }

    #[test]
    fn bounds_the_grid_in_very_large_windows() {
        assert!(build(8000., 8000.).unwrap().cells.len() <= 160 * 80);
    }

    #[test]
    fn splash_colours_match_the_browser() {
        // buzz-app's computed --splash-start in light mode.
        let light = oklch(relative(ART_START, 1.29, 0.));
        assert!((light[0] - 0.7108).abs() < 0.002, "{light:?}");
        assert!((light[1] - 0.2463).abs() < 0.002, "{light:?}");
        assert!((light[2] - 45.01).abs() < 0.5, "{light:?}");
        let dark = oklch(relative(ART_END, 1.21, 1.));
        assert!((dark[1] - 0.1902).abs() < 0.002, "{dark:?}");
    }

    #[test]
    fn clicks_keep_three_ripples() {
        let mut splash = Splash::new();
        for _ in 0..5 {
            splash.click(0.5, 0.5);
        }
        assert_eq!(splash.pulses.len(), 3);
    }

    #[test]
    fn hatching_stays_inside_the_hexagon() {
        let hexagon: Vec<(f32, f32)> = (0..6)
            .map(|corner| {
                let angle = corner as f32 * PI / 3. - PI / 2.;
                (angle.cos() * 5., angle.sin() * 5.)
            })
            .collect();
        let (a, b) = clip((-10., 0.), (10., 0.), &hexagon).unwrap();
        let half = 5. * 3f32.sqrt() / 2.;
        assert!((a.0 + half).abs() < 1e-4 && (b.0 - half).abs() < 1e-4);
        assert_eq!(clip((-10., 6.), (10., 6.), &hexagon), None);
    }
}
