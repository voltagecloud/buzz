//! The shell backdrop (`--bg-app` = `--gradient-1`): "Sky field" in light
//! mode, "Night garden" in dark mode.
//!
//! GPUI only paints two-stop linear gradients, so the CSS layers are
//! rasterized here, following the CSS Images 3 geometry, into a bitmap at
//! half the window size and stretched over the window. The result is
//! cached per size and appearance.

use crate::{ActiveTheme, Appearance, theme::Theme};
use gpui_kit::{
    AnyElement, App, Global, ImageSource, IntoElement, ObjectFit, ParentElement, RenderImage,
    RenderOnce, Styled, StyledImage, Window, div, img,
};
use smallvec::SmallVec;
use std::sync::Arc;

/// Premultiplied linear-light-free sRGB color, as CSS interpolates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Premul([f32; 4]);

impl Premul {
    fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        let f = |c: u8| c as f32 / 255. * a;
        Self([f(r), f(g), f(b), a])
    }

    fn hex(hex: u32) -> Self {
        Self::rgba((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 1.)
    }

    const TRANSPARENT: Self = Self([0.; 4]);

    fn lerp(self, other: Self, t: f32) -> Self {
        let mut out = [0.; 4];
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.0[i] + (other.0[i] - self.0[i]) * t;
        }
        Self(out)
    }

    /// `self` over `below`.
    fn over(self, below: Self) -> Self {
        let a = self.0[3];
        let mut out = [0.; 4];
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.0[i] + below.0[i] * (1. - a);
        }
        Self(out)
    }
}

/// Evaluates color stops at gradient position `t`.
fn stops_at(stops: &[(Premul, f32)], t: f32) -> Premul {
    if t <= stops[0].1 {
        return stops[0].0;
    }
    for pair in stops.windows(2) {
        let ((c0, p0), (c1, p1)) = (pair[0], pair[1]);
        if t <= p1 {
            let span = p1 - p0;
            return if span <= 0. {
                c1
            } else {
                c0.lerp(c1, (t - p0) / span)
            };
        }
    }
    stops[stops.len() - 1].0
}

enum RadialSize {
    /// Ellipse sized to the farthest corner, with the closest-side aspect
    /// ratio (the CSS default).
    FarthestCorner,
    /// Explicit radii as fractions of the box width and height.
    Explicit(f32, f32),
}

enum Layer {
    Linear {
        angle_deg: f32,
        stops: Vec<(Premul, f32)>,
    },
    Radial {
        at: (f32, f32),
        size: RadialSize,
        stops: Vec<(Premul, f32)>,
    },
}

impl Layer {
    /// Precomputes geometry for a `w`×`h` box and returns a sampler.
    fn sampler(&self, w: f32, h: f32) -> Box<dyn Fn(f32, f32) -> Premul + '_> {
        match self {
            Layer::Linear { angle_deg, stops } => {
                let a = angle_deg.to_radians();
                let (dx, dy) = (a.sin(), -a.cos());
                let len = (w * a.sin()).abs() + (h * a.cos()).abs();
                let (cx, cy) = (w / 2., h / 2.);
                Box::new(move |x, y| {
                    let t = ((x - cx) * dx + (y - cy) * dy) / len + 0.5;
                    stops_at(stops, t)
                })
            }
            Layer::Radial { at, size, stops } => {
                let (cx, cy) = (at.0 * w, at.1 * h);
                let (rx, ry) = match size {
                    RadialSize::Explicit(fx, fy) => (fx * w, fy * h),
                    RadialSize::FarthestCorner => {
                        let ratio = cx.min(w - cx) / cy.min(h - cy);
                        let fx = cx.max(w - cx);
                        let fy = cy.max(h - cy);
                        let ry = ((fx / ratio).powi(2) + fy.powi(2)).sqrt();
                        (ry * ratio, ry)
                    }
                };
                Box::new(move |x, y| {
                    let t = (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt();
                    stops_at(stops, t)
                })
            }
        }
    }
}

/// The CSS layers, bottom first.
fn layers(appearance: Appearance) -> Vec<Layer> {
    let transparent = Premul::TRANSPARENT;
    match appearance {
        // radial-gradient(at 88% 84%, #f5e99e 0%, transparent 60%),
        // linear-gradient(148deg, #f8fbff 0%, #c6e8fb 50%, #d9f0d5 100%)
        Appearance::Light => vec![
            Layer::Linear {
                angle_deg: 148.,
                stops: vec![
                    (Premul::hex(0xf8fbff), 0.),
                    (Premul::hex(0xc6e8fb), 0.5),
                    (Premul::hex(0xd9f0d5), 1.),
                ],
            },
            Layer::Radial {
                at: (0.88, 0.84),
                size: RadialSize::FarthestCorner,
                stops: vec![(Premul::hex(0xf5e99e), 0.), (transparent, 0.6)],
            },
        ],
        Appearance::Dark => vec![
            Layer::Linear {
                angle_deg: 153.,
                stops: vec![
                    (Premul::hex(0x071311), 0.),
                    (Premul::hex(0x0a2929), 0.51),
                    (Premul::hex(0x081722), 1.),
                ],
            },
            Layer::Radial {
                at: (0.52, 0.48),
                size: RadialSize::FarthestCorner,
                stops: vec![(Premul::rgba(6, 95, 70, 0.30), 0.), (transparent, 0.78)],
            },
            Layer::Radial {
                at: (0.12, 0.86),
                size: RadialSize::FarthestCorner,
                stops: vec![(Premul::rgba(56, 189, 248, 0.52), 0.), (transparent, 0.46)],
            },
            Layer::Radial {
                at: (0.88, 0.16),
                size: RadialSize::FarthestCorner,
                stops: vec![(Premul::rgba(52, 211, 153, 0.56), 0.), (transparent, 0.42)],
            },
            Layer::Radial {
                at: (0.52, 0.5),
                size: RadialSize::Explicit(1.1, 1.),
                stops: vec![(transparent, 0.34), (Premul::rgba(2, 11, 13, 0.42), 1.)],
            },
        ],
    }
}

/// Renders the backdrop for a `width`×`height` CSS-pixel window into a
/// `width/scale`×`height/scale` BGRA bitmap.
fn rasterize(appearance: Appearance, width: f32, height: f32, scale: f32) -> RenderImage {
    let w = ((width / scale).ceil() as u32).max(1);
    let h = ((height / scale).ceil() as u32).max(1);
    let layers = layers(appearance);
    let samplers: Vec<_> = layers.iter().map(|l| l.sampler(width, height)).collect();
    let mut bytes = Vec::with_capacity((w * h * 4) as usize);
    for py in 0..h {
        for px in 0..w {
            // Sample at the pixel center, in window coordinates.
            let (x, y) = ((px as f32 + 0.5) * scale, (py as f32 + 0.5) * scale);
            let mut color = Premul::TRANSPARENT;
            for sample in &samplers {
                color = sample(x, y).over(color);
            }
            // Ordered dither against 8-bit banding on the long dark ramps.
            let dither = (((px * 7 + py * 13) % 16) as f32 / 16. - 0.5) / 255.;
            let byte = |c: f32| ((c + dither).clamp(0., 1.) * 255. + 0.5) as u8;
            let [r, g, b, a] = color.0;
            bytes.extend_from_slice(&[byte(b), byte(g), byte(r), byte(a)]);
        }
    }
    let buffer = image::RgbaImage::from_raw(w, h, bytes).expect("bitmap size");
    RenderImage::new(SmallVec::from_elem(image::Frame::new(buffer), 1))
}

#[derive(Default)]
struct BackdropCache {
    key: Option<(Appearance, u32, u32)>,
    image: Option<Arc<RenderImage>>,
}

impl Global for BackdropCache {}

/// Downsampling factor for the backdrop bitmap.
const BACKDROP_SCALE: f32 = 2.;

fn backdrop(appearance: Appearance, window: &mut Window, cx: &mut App) -> Arc<RenderImage> {
    let size = window.viewport_size();
    let (w, h) = (f32::from(size.width), f32::from(size.height));
    let key = (appearance, w.round() as u32, h.round() as u32);
    let cache = cx.default_global::<BackdropCache>();
    if cache.key == Some(key)
        && let Some(image) = &cache.image
    {
        return image.clone();
    }
    let image = Arc::new(rasterize(appearance, w, h, BACKDROP_SCALE));
    let old = cache.image.replace(image.clone());
    cache.key = Some(key);
    if let Some(old) = old {
        cx.drop_image(old, Some(window));
    }
    image
}

/// The full-window shell background (`.shell-background`): the `--bg-app`
/// gradient over the `--workspace` color, with its children on top.
#[derive(IntoElement, Default)]
pub struct ShellBackground {
    children: Vec<AnyElement>,
}

impl ShellBackground {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ParentElement for ShellBackground {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for ShellBackground {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let appearance = Theme::global(cx).appearance();
        let workspace = cx.colors().workspace;
        let image = backdrop(appearance, window, cx);
        div()
            .size_full()
            .relative()
            .bg(workspace)
            .child(
                img(ImageSource::Render(image))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(ObjectFit::Fill),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .children(self.children),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(appearance: Appearance, x: f32, y: f32) -> [f32; 4] {
        let layers = layers(appearance);
        let mut color = Premul::TRANSPARENT;
        for layer in &layers {
            color = layer.sampler(1200., 800.)(x, y).over(color);
        }
        color.0.map(|c| c * 255.)
    }

    #[test]
    fn light_corners_follow_the_linear_ramp() {
        // Top-left is the first stop, slightly tinted by the yellow glow's
        // transparent tail; it should stay near #f8fbff.
        let [r, g, b, a] = pixel(Appearance::Light, 0., 0.);
        assert!((r - 248.).abs() < 3. && (g - 251.).abs() < 3. && (b - 255.).abs() < 3.);
        assert_eq!(a, 255.);
        // The glow center is the pure glow color.
        let [r, g, b, _] = pixel(Appearance::Light, 1056., 672.);
        assert!((r - 245.).abs() < 1. && (g - 233.).abs() < 1. && (b - 158.).abs() < 1.);
    }
}
