//! The pairing QR code, drawn as buzz-app draws its SVG
//! (`src-tauri/src/pairing/qr.rs`): 240px on white, round dots, three
//! rounded finder patterns, and the app icon in the middle. Rows of dots
//! pop in top to bottom over a quarter second (`qr-reveal.css`), unless
//! the system asks for reduced motion.

use buzz_pair_mobile::{
    QrMatrix,
    qr::{QUIET, reveal},
};
use gpui_kit::*;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

/// `size-60`.
const SIDE: Rems = rems(15.);

static ICON: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../../../../assets/icons/buzz-256.png").to_vec(),
    ))
});

/// When the code on screen first appeared, so its reveal plays once per
/// code, including each renewal.
#[derive(Default)]
pub(super) struct Reveal(Option<(Arc<QrMatrix>, Instant)>);

impl Reveal {
    /// Milliseconds since `matrix` appeared, or `None` once its dots have
    /// all landed (or motion is reduced). Asks for the next frame while
    /// the reveal plays.
    pub(super) fn elapsed(
        &mut self,
        matrix: &Arc<QrMatrix>,
        window: &mut Window,
        cx: &App,
    ) -> Option<f32> {
        match &self.0 {
            Some((shown, _)) if Arc::ptr_eq(shown, matrix) => {}
            _ => self.0 = Some((matrix.clone(), Instant::now())),
        }
        let elapsed = self.0.as_ref()?.1.elapsed().as_secs_f32() * 1000.;
        if cx.reduce_motion() || elapsed >= matrix.reveal_ms() {
            return None;
        }
        window.request_animation_frame();
        Some(elapsed)
    }
}

pub(super) fn qr_code(matrix: Arc<QrMatrix>, elapsed: Option<f32>) -> impl IntoElement {
    let extent = matrix.extent() as f32;
    let at = |modules: f32| SIDE * ((modules + QUIET as f32) / extent);
    let icon_start = at(matrix.icon_start);
    let icon_side = SIDE * (matrix.icon_size / extent);
    let painted = matrix.clone();
    div()
        .id("pairing-qr")
        .relative()
        .flex_none()
        .size(SIDE)
        .bg(white())
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| paint(&painted, elapsed, bounds, window),
            )
            .size_full(),
        )
        .child(
            img(ICON.clone())
                .absolute()
                .left(icon_start)
                .top(icon_start)
                .size(icon_side)
                .rounded(SIDE * (2. / extent)),
        )
}

fn paint(matrix: &QrMatrix, elapsed: Option<f32>, bounds: Bounds<Pixels>, window: &mut Window) {
    let module = bounds.size.width / matrix.extent() as f32;
    let origin = bounds.origin + point(module * QUIET as f32, module * QUIET as f32);
    let square = |x: f32, y: f32, side: f32| {
        Bounds::new(
            origin + point(module * x, module * y),
            size(module * side, module * side),
        )
    };
    let black = black();
    for &(col, row) in &matrix.dots {
        let (opacity, scale) = match elapsed {
            Some(elapsed) => reveal(elapsed - matrix.delay(row)),
            None => (1., 1.),
        };
        if opacity <= 0. || scale <= 0. {
            continue;
        }
        let diameter = 0.8 * scale;
        let inset = (1. - diameter) / 2.;
        window.paint_quad(
            fill(
                square(col as f32 + inset, row as f32 + inset, diameter),
                black.opacity(opacity),
            )
            .corner_radii(module * (diameter / 2.)),
        );
    }
    for (x, y) in matrix.finders() {
        let (x, y) = (x as f32, y as f32);
        window.paint_quad(fill(square(x, y, 7.), black).corner_radii(module * 2.));
        window.paint_quad(fill(square(x + 1., y + 1., 5.), white()).corner_radii(module * 0.8));
        window.paint_quad(fill(square(x + 2., y + 2., 3.), black).corner_radii(module * 0.4));
    }
    window.paint_quad(
        fill(
            square(matrix.icon_start, matrix.icon_start, matrix.icon_size),
            black,
        )
        .corner_radii(module * 2.),
    );
}
