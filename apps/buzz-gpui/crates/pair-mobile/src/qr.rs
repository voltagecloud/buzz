//! The pairing QR code as a matrix for the UI to draw. The geometry is
//! buzz-app's (`src-tauri/src/pairing/qr.rs`, after the original Buzz
//! StyledQrCode): round dots, three rounded finder patterns, a four-module
//! quiet zone, and the app icon over a cleared center that error
//! correction level M still covers.

use qrcode::{Color, EcLevel, QrCode};

/// Light modules around the code, on every side.
pub const QUIET: usize = 4;
/// How long one dot's reveal lasts (`qr-reveal.css`).
pub const DOT_MS: f32 = 58.0;

#[derive(Clone, Debug, PartialEq)]
pub struct QrMatrix {
    /// Modules per side, without the quiet zone.
    pub size: usize,
    /// Dark data modules, drawn as dots: `(column, row)`. Finder patterns
    /// and the icon area are left out.
    pub dots: Vec<(u16, u16)>,
    /// The icon square: its first module and its side, in modules.
    pub icon_start: f32,
    pub icon_size: f32,
}

impl QrMatrix {
    pub fn encode(value: &str) -> Result<Self, String> {
        // The original component's correction level and bounded logo area.
        let code = QrCode::with_error_correction_level(value.as_bytes(), EcLevel::M)
            .map_err(|_| "Couldn’t create the pairing code.")?;
        let size = code.width();
        let mut center = ((size * size) as f64 * 0.1).sqrt().floor() as usize;
        if center.is_multiple_of(2) {
            center -= 1;
        }
        let start = (size - center) / 2;
        let end = start + center;
        let icon_size = center as f32 * 0.8;
        let icon_start = (size as f32 - icon_size) / 2.0;
        let mut dots = Vec::new();
        for row in 0..size {
            for col in 0..size {
                let finder =
                    (row < 7 && (col < 7 || col >= size - 7)) || (row >= size - 7 && col < 7);
                let logo = (start..end).contains(&row) && (start..end).contains(&col);
                if code[(col, row)] == Color::Dark && !finder && !logo {
                    dots.push((col as u16, row as u16));
                }
            }
        }
        Ok(Self {
            size,
            dots,
            icon_start,
            icon_size,
        })
    }

    /// Side of the drawing in modules, quiet zone included.
    pub fn extent(&self) -> usize {
        self.size + 2 * QUIET
    }

    /// Top-left module of each 7×7 finder pattern.
    pub fn finders(&self) -> [(usize, usize); 3] {
        [(0, 0), (self.size - 7, 0), (0, self.size - 7)]
    }

    /// When a row's dots start to appear, in milliseconds: the original
    /// 250ms reveal is row travel plus one dot's animation.
    pub fn delay(&self, row: u16) -> f32 {
        (row as f64 / self.size as f64 * (250.0 / 1.3)).round() as f32
    }

    /// When the last dot has finished appearing, in milliseconds.
    pub fn reveal_ms(&self) -> f32 {
        self.delay(self.size as u16 - 1) + DOT_MS
    }
}

/// `buzz-qr-cell-reveal` (`qr-reveal.css`, linear timing): a dot's
/// opacity and scale `elapsed` milliseconds after its row's delay.
pub fn reveal(elapsed: f32) -> (f32, f32) {
    const SCALE: [(f32, f32); 9] = [
        (0.0, 0.0),
        (0.27, 0.0),
        (0.30, 0.043),
        (0.35, 0.167),
        (0.40, 0.358),
        (0.45, 0.632),
        (0.49, 0.918),
        (0.50, 1.1),
        (1.0, 1.0),
    ];
    let progress = (elapsed / DOT_MS).clamp(0.0, 1.0);
    // Opacity runs 0 to 1 in step with the keyframe offsets.
    let scale = SCALE
        .windows(2)
        .find(|pair| progress <= pair[1].0)
        .map(|pair| {
            let ((from, a), (to, b)) = (pair[0], pair[1]);
            a + (b - a) * (progress - from) / (to - from)
        })
        .unwrap_or(1.0);
    (progress, scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draws the matrix as a phone camera would see it: module squares,
    /// the finder patterns, and the icon as a dark square.
    fn scan(matrix: &QrMatrix) -> String {
        const PX: usize = 6;
        let side = matrix.extent() * PX;
        let mut dark = vec![false; side * side];
        let mut fill = |x0: f32, y0: f32, w: f32, value: bool| {
            let (x0, y0) = (
                (x0 + QUIET as f32) * PX as f32,
                (y0 + QUIET as f32) * PX as f32,
            );
            let w = w * PX as f32;
            for y in (y0 as usize)..((y0 + w) as usize) {
                for x in (x0 as usize)..((x0 + w) as usize) {
                    dark[y * side + x] = value;
                }
            }
        };
        for &(col, row) in &matrix.dots {
            fill(col as f32, row as f32, 1.0, true);
        }
        for (x, y) in matrix.finders() {
            fill(x as f32, y as f32, 7.0, true);
            fill(x as f32 + 1.0, y as f32 + 1.0, 5.0, false);
            fill(x as f32 + 2.0, y as f32 + 2.0, 3.0, true);
        }
        fill(matrix.icon_start, matrix.icon_start, matrix.icon_size, true);
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
            if dark[y * side + x] { 0 } else { 255 }
        });
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1, "one code is visible");
        grids[0].decode().expect("the code scans").1
    }

    #[test]
    fn styled_code_still_scans_with_its_icon() {
        let (_session, qr) =
            buzz_pairing::PairingSession::new_source("wss://relay.example.com/pair".into());
        let uri = buzz_pairing::qr::encode_qr(&qr);
        let matrix = QrMatrix::encode(&uri).unwrap();
        let scanned = scan(&matrix);
        assert_eq!(scanned, uri);
        assert_eq!(
            buzz_pairing::qr::decode_qr(&scanned).unwrap().source_pubkey,
            qr.source_pubkey
        );
        // The icon area is cleared, and it is centered.
        let center = matrix.size as f32 / 2.0;
        assert!((matrix.icon_start + matrix.icon_size / 2.0 - center).abs() < 1e-4);
        assert!(!matrix.dots.iter().any(|&(c, r)| {
            let inside = |v: u16| {
                (v as f32) >= matrix.icon_start + 1.0
                    && (v as f32) < matrix.icon_start + matrix.icon_size - 1.0
            };
            inside(c) && inside(r)
        }));
        // Finder patterns are drawn on their own, not as dots.
        assert!(!matrix.dots.iter().any(|&(c, r)| c < 7 && r < 7));
    }

    #[test]
    fn rows_reveal_top_to_bottom_within_a_quarter_second() {
        let matrix = QrMatrix::encode("pairing-test").unwrap();
        assert_eq!(matrix.delay(0), 0.0);
        assert!(matrix.delay(1) > 0.0);
        assert!(matrix.reveal_ms() <= 250.0);
        assert_eq!(reveal(-5.0), (0.0, 0.0));
        assert_eq!(reveal(0.0), (0.0, 0.0));
        // The dot overshoots to 110% halfway, then settles.
        let (opacity, scale) = reveal(DOT_MS / 2.0);
        assert!((opacity - 0.5).abs() < 1e-6 && (scale - 1.1).abs() < 1e-6);
        assert_eq!(reveal(DOT_MS), (1.0, 1.0));
        assert_eq!(reveal(1000.0), (1.0, 1.0));
    }
}
