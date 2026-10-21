//! The launch surface (buzz-app's `index.html` `#buzz-launch`, `launch.ts`
//! and `startup.css`): the animated Buzz mark over the workspace colour,
//! with the window controls in a 48px draggable header
//! (`LoadingWindowHeader`), shown until the first content is ready.
//!
//! Unlike buzz-app, it goes as soon as nothing holds it, with no fade and
//! no full loop of the mark first: content is usually ready within a few
//! hundred milliseconds, well inside the 1.76s loop buzz-app waits out.
//! Content still settling (the sidebar and timeline) holds it for at most
//! a second; content required to show anything (the open channel's first
//! page) holds it until it loads or fails.
//!
//! The mark is buzz-app's `buzz-loading-mark.svg`, whose SMIL tracks are
//! played here: every track is a 1.76s spline animation of one attribute
//! of an ellipse, circle or rounded rectangle. Cut-outs are painted in the
//! background colour, which is what the SVG mask shows on the solid
//! launch surface.

use super::window_controls;
use buzz_ui::ActiveTheme;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One loop of the mark (`CYCLE_MS`).
pub const CYCLE: Duration = Duration::from_millis(1760);
/// How long settling content may hold the surface.
pub const SETTLING_BUDGET: Duration = Duration::from_secs(1);
/// The mark is drawn 72px square (`4.5rem`).
const MARK_SIZE: f32 = 72.;

const SOURCE: &str = include_str!("../../../../assets/launch/buzz-loading-mark.svg");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ellipse,
    Circle,
    Rect,
}

#[derive(Debug)]
struct Track {
    attribute: String,
    times: Vec<f32>,
    values: Vec<f32>,
    splines: Vec<[f32; 4]>,
}

#[derive(Debug)]
struct Shape {
    kind: Kind,
    /// Part of the mask: painted in the background colour.
    cutout: bool,
    base: HashMap<String, f32>,
    tracks: Vec<Track>,
}

fn numbers(text: &str, separator: char) -> Vec<f32> {
    text.split(separator)
        .filter_map(|value| value.trim().parse().ok())
        .collect()
}

fn parse(source: &str) -> Vec<Shape> {
    let Ok(document) = roxmltree::Document::parse(source) else {
        return Vec::new();
    };
    let mut shapes = Vec::new();
    for node in document.descendants() {
        let kind = match node.tag_name().name() {
            "ellipse" => Kind::Ellipse,
            "circle" => Kind::Circle,
            "rect" => Kind::Rect,
            _ => continue,
        };
        let cutout = node.ancestors().any(|a| a.tag_name().name() == "mask");
        // The mask's full-size white rect only enables it.
        if cutout && node.attribute("fill") == Some("#fff") {
            continue;
        }
        let base = node
            .attributes()
            .filter_map(|a| Some((a.name().to_string(), a.value().parse().ok()?)))
            .collect();
        let tracks = node
            .children()
            .filter(|child| child.tag_name().name() == "animate")
            .filter_map(|animate| {
                let splines = animate
                    .attribute("keySplines")?
                    .split(';')
                    .map(|spline| {
                        let v = numbers(spline.trim(), ' ');
                        [
                            v.first().copied().unwrap_or(0.),
                            v.get(1).copied().unwrap_or(0.),
                            v.get(2).copied().unwrap_or(1.),
                            v.get(3).copied().unwrap_or(1.),
                        ]
                    })
                    .collect();
                Some(Track {
                    attribute: animate.attribute("attributeName")?.to_string(),
                    times: numbers(animate.attribute("keyTimes")?, ';'),
                    values: numbers(animate.attribute("values")?, ';'),
                    splines,
                })
            })
            .collect();
        shapes.push(Shape {
            kind,
            cutout,
            base,
            tracks,
        });
    }
    shapes
}

fn shapes() -> &'static [Shape] {
    static SHAPES: OnceLock<Vec<Shape>> = OnceLock::new();
    SHAPES.get_or_init(|| parse(SOURCE))
}

/// `cubic-bezier(x1, y1, x2, y2)` at progress `x`.
fn ease([x1, y1, x2, y2]: [f32; 4], x: f32) -> f32 {
    let curve = |a: f32, b: f32, s: f32| {
        let t = 1. - s;
        3. * t * t * s * a + 3. * t * s * s * b + s * s * s
    };
    // Bisection is exact enough for 60 frames a second.
    let (mut low, mut high) = (0f32, 1f32);
    for _ in 0..24 {
        let mid = (low + high) / 2.;
        if curve(x1, x2, mid) < x {
            low = mid;
        } else {
            high = mid;
        }
    }
    curve(y1, y2, (low + high) / 2.)
}

impl Track {
    /// The value at `t`, the fraction of the cycle.
    fn at(&self, t: f32) -> Option<f32> {
        let last = self.times.len().checked_sub(1)?;
        if self.values.len() <= last {
            return None;
        }
        let segment = (0..last)
            .find(|&i| t < self.times[i + 1])
            .unwrap_or(last.saturating_sub(1));
        let (start, end) = (self.times[segment], self.times[segment + 1]);
        let local = if end > start {
            ((t - start) / (end - start)).clamp(0., 1.)
        } else {
            1.
        };
        let eased = self
            .splines
            .get(segment)
            .map_or(local, |spline| ease(*spline, local));
        let (from, to) = (self.values[segment], self.values[segment + 1]);
        Some(from + (to - from) * eased)
    }
}

impl Shape {
    fn value(&self, attribute: &str, t: f32) -> f32 {
        self.tracks
            .iter()
            .find(|track| track.attribute == attribute)
            .and_then(|track| track.at(t))
            .or_else(|| self.base.get(attribute).copied())
            .unwrap_or(0.)
    }
}

/// The SVG's viewBox.
const VIEW: (f32, f32, f32, f32) = (-16., -16., 498., 341.);

/// Paints the mark at fraction `t` of its cycle into `bounds`.
fn paint_mark(t: f32, bounds: Bounds<Pixels>, ink: Hsla, background: Hsla, window: &mut Window) {
    let scale = (f32::from(bounds.size.width) / VIEW.2).min(f32::from(bounds.size.height) / VIEW.3);
    let offset = point(
        bounds.origin.x + (bounds.size.width - px(VIEW.2 * scale)) / 2.,
        bounds.origin.y + (bounds.size.height - px(VIEW.3 * scale)) / 2.,
    );
    let at = |x: f32, y: f32| {
        point(
            offset.x + px((x - VIEW.0) * scale),
            offset.y + px((y - VIEW.1) * scale),
        )
    };
    // Mask cut-outs come last, over the shapes they cut.
    let order = shapes()
        .iter()
        .filter(|shape| !shape.cutout)
        .chain(shapes().iter().filter(|shape| shape.cutout));
    for shape in order {
        let opacity = if shape.base.contains_key("opacity")
            || shape.tracks.iter().any(|t| t.attribute == "opacity")
        {
            shape.value("opacity", t).clamp(0., 1.)
        } else {
            1.
        };
        if opacity <= 0. {
            continue;
        }
        let color = if shape.cutout { background } else { ink };
        let color = color.opacity(opacity);
        let mut path = PathBuilder::fill();
        match shape.kind {
            Kind::Ellipse | Kind::Circle => {
                let (cx, cy) = (shape.value("cx", t), shape.value("cy", t));
                let (rx, ry) = match shape.kind {
                    Kind::Circle => (shape.value("r", t), shape.value("r", t)),
                    _ => (shape.value("rx", t), shape.value("ry", t)),
                };
                if rx <= 0. || ry <= 0. {
                    continue;
                }
                ellipse(&mut path, cx, cy, rx, ry, &at);
            }
            Kind::Rect => {
                let (x, y) = (shape.value("x", t), shape.value("y", t));
                let (w, h) = (shape.value("width", t), shape.value("height", t));
                if w <= 0. || h <= 0. {
                    continue;
                }
                let r = shape.value("rx", t).min(w / 2.).min(h / 2.).max(0.);
                rounded_rect(&mut path, x, y, w, h, r, &at);
            }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

/// Cubic approximation of a quarter ellipse.
const KAPPA: f32 = 0.552_284_8;

fn ellipse(
    path: &mut PathBuilder,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    at: &impl Fn(f32, f32) -> Point<Pixels>,
) {
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    path.move_to(at(cx + rx, cy));
    path.cubic_bezier_to(at(cx, cy + ry), at(cx + rx, cy + ky), at(cx + kx, cy + ry));
    path.cubic_bezier_to(at(cx - rx, cy), at(cx - kx, cy + ry), at(cx - rx, cy + ky));
    path.cubic_bezier_to(at(cx, cy - ry), at(cx - rx, cy - ky), at(cx - kx, cy - ry));
    path.cubic_bezier_to(at(cx + rx, cy), at(cx + kx, cy - ry), at(cx + rx, cy - ky));
    path.close();
}

fn rounded_rect(
    path: &mut PathBuilder,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    at: &impl Fn(f32, f32) -> Point<Pixels>,
) {
    let k = r * KAPPA;
    path.move_to(at(x + r, y));
    path.line_to(at(x + w - r, y));
    path.cubic_bezier_to(at(x + w, y + r), at(x + w - r + k, y), at(x + w, y + r - k));
    path.line_to(at(x + w, y + h - r));
    path.cubic_bezier_to(
        at(x + w - r, y + h),
        at(x + w, y + h - r + k),
        at(x + w - r + k, y + h),
    );
    path.line_to(at(x + r, y + h));
    path.cubic_bezier_to(at(x, y + h - r), at(x + r - k, y + h), at(x, y + h - r + k));
    path.line_to(at(x, y + r));
    path.cubic_bezier_to(at(x + r, y), at(x, y + r - k), at(x + r - k, y));
    path.close();
}

/// The Buzz mark `width` wide, playing from `since`, its cut-outs in
/// `background`: the thread loading mark (`ThreadLoading`). With reduced
/// motion it holds its first frame, as `buzz-mark.svg` does.
pub fn mark(since: Instant, width: Pixels, background: Hsla, window: &mut Window, cx: &App) -> Div {
    let phase = if cx.reduce_motion() {
        0.
    } else {
        window.request_animation_frame();
        let elapsed = since.elapsed().as_secs_f32();
        (elapsed / CYCLE.as_secs_f32()).fract()
    };
    let ink = if cx.theme().is_dark() {
        gpui_kit::white()
    } else {
        gpui_kit::black()
    };
    div().w(width).h(width * (VIEW.3 / VIEW.2)).child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| paint_mark(phase, bounds, ink, background, window),
        )
        .size_full(),
    )
}

/// Whether the launch surface still shows.
#[derive(Clone, Debug)]
pub struct Launch {
    started: Instant,
    /// Nothing holds the surface any more.
    done: bool,
}

/// What the app shows under the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// Content required to show anything is still loading.
    Required,
    /// Content that may keep loading under the app is still settling.
    Settling,
    Ready,
}

impl Launch {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            done: false,
        }
    }

    /// Takes the content state at `now`; the surface goes once nothing
    /// holds it.
    pub fn update(&mut self, content: Content, now: Instant) {
        self.done = self.done
            || match content {
                Content::Required => false,
                Content::Settling => now >= self.started + SETTLING_BUDGET,
                Content::Ready => true,
            };
    }

    /// Whether the surface still shows.
    pub fn showing(&self) -> bool {
        !self.done
    }

    /// The mark's position in its loop, 0..1.
    fn phase(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        (elapsed / CYCLE.as_secs_f32()).fract()
    }

    /// The surface: the mark centred, and the window controls in a 48px
    /// draggable header. Asks for the next frame while it shows.
    pub fn render(&self, window: &mut Window, cx: &App) -> Option<AnyElement> {
        if !self.showing() {
            return None;
        }
        let now = Instant::now();
        window.request_animation_frame();
        let c = cx.colors();
        let background = c.surface_base;
        let ink = if cx.theme().is_dark() {
            gpui_kit::white()
        } else {
            gpui_kit::black()
        };
        let phase = self.phase(now);
        let mark = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| paint_mark(phase, bounds, ink, background, window),
        )
        .size(px(MARK_SIZE));
        Some(
            div()
                .id("buzz-launch")
                .absolute()
                .inset_0()
                .occlude()
                .bg(background)
                .flex()
                .items_center()
                .justify_center()
                .child(mark)
                .child(
                    window_controls::title_bar(
                        div()
                            .id("launch-header")
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(rems(3.))
                            .px(rems(1.))
                            .flex()
                            .items_center()
                            .justify_end(),
                    )
                    .child(window_controls::not_title_bar(div()).child(
                        window_controls::render(window_controls::Policy::current(), window),
                    )),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn reads_every_track_of_the_mark() {
        let shapes = shapes();
        assert_eq!(shapes.iter().filter(|s| s.cutout).count(), 4);
        assert_eq!(shapes.iter().filter(|s| !s.cutout).count(), 3);
        let tracks: usize = shapes.iter().map(|s| s.tracks.len()).sum();
        assert_eq!(tracks, 26);
        assert!(
            shapes.iter().flat_map(|s| &s.tracks).all(|t| {
                t.times.len() == t.values.len() && t.splines.len() + 1 == t.times.len()
            })
        );
    }

    #[test]
    fn tracks_follow_their_keyframes() {
        let track = Track {
            attribute: "x".into(),
            times: vec![0., 0.5, 1.],
            values: vec![0., 10., 0.],
            splines: vec![[0., 0., 1., 1.], [0., 0., 1., 1.]],
        };
        assert!(track.at(0.).unwrap().abs() < 0.01);
        assert!((track.at(0.25).unwrap() - 5.).abs() < 0.01);
        assert!((track.at(0.5).unwrap() - 10.).abs() < 0.01);
        assert!((track.at(1.).unwrap()).abs() < 0.01);
        // The big rounded square starts and ends the loop collapsed.
        let square = shapes()
            .iter()
            .find(|s| !s.cutout && s.kind == Kind::Rect)
            .unwrap();
        assert!(square.value("width", 0.) < 0.5);
        assert!((square.value("width", 0.5) - 210.).abs() < 0.5);
    }

    #[test]
    fn goes_once_ready_with_a_settling_budget() {
        let start = Instant::now();
        // Required content holds it however long it takes.
        let mut launch = Launch::new(start);
        launch.update(Content::Required, start + Duration::from_secs(30));
        assert!(launch.showing());
        // Ready content ends it at once, mid-loop.
        let mut launch = Launch::new(start);
        launch.update(Content::Ready, start + Duration::from_millis(100));
        assert!(!launch.showing());
        // Settling content holds it until the budget runs out.
        let mut launch = Launch::new(start);
        launch.update(Content::Settling, start + Duration::from_millis(500));
        assert!(launch.showing());
        launch.update(Content::Settling, start + SETTLING_BUDGET);
        assert!(!launch.showing());
        // Once gone, it stays gone.
        launch.update(Content::Required, start + SETTLING_BUDGET * 2);
        assert!(!launch.showing());
    }
}
