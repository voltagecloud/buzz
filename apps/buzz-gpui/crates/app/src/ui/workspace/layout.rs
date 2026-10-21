//! Breakpoints and sizes of the shell (ui.md §2.8, §2.10, §2.11). All in
//! logical pixels, as buzz-app's media queries are.

/// At or below this width the sidebar becomes a drawer, the header pages
/// fold into More pages and the dock fills the width.
pub const NARROW: f32 = 650.;
/// At or below this width the dock overlays the page.
pub const OVERLAY: f32 = 1000.;
/// The Messages board overlays its tab pane when it is this narrow, as a
/// wide sidebar leaves less room than the window width suggests.
pub const BOARD_OVERLAY: f32 = 651.;

pub const RAIL: f32 = 48.;
pub const HEADER_GAP: f32 = 8.;

pub const SIDEBAR_DEFAULT: f32 = 260.;
pub const SIDEBAR_MIN: f32 = 220.;
pub const SIDEBAR_MAX: f32 = 520.;
/// The main region never gets narrower than this beside the sidebar.
pub const MAIN_MIN: f32 = 336.;
/// One arrow-key step of a resize handle, and with Shift.
pub const STEP: f32 = 16.;
pub const LARGE_STEP: f32 = 48.;

pub fn narrow(width: f32) -> bool {
    width <= NARROW
}

/// The header's side paddings: 16 left (8 at 700px and below), 16 right
/// from 640px (8 below).
pub fn header_padding(width: f32) -> (f32, f32) {
    let left = if width <= 700. { 8. } else { 16. };
    (left, gutter(width))
}

/// The body's right and bottom gutters around the joined panel.
pub fn gutter(width: f32) -> f32 {
    if width >= 640. { 16. } else { 8. }
}

/// The sidebar width to use: rounded, within 220..520, and leaving the
/// main region at least 336px of `content` (the joined panel's width).
pub fn clamp_sidebar(width: f32, content: f32) -> f32 {
    let max = SIDEBAR_MAX.min(SIDEBAR_MIN.max(content - MAIN_MIN));
    width.round().clamp(SIDEBAR_MIN, max)
}

/// Whether the header pages fold into More pages: on a narrow window, or
/// when the centred pills would run into either side group
/// (`2 * (max(left, right) + gap) + center > width`). `left` and `right`
/// include the header's paddings.
pub fn compact_pages(width: f32, left: f32, right: f32, center: f32) -> bool {
    narrow(width) || 2. * (left.max(right) + HEADER_GAP) + center > width
}

/// How a page and its dock share the main region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DockMode {
    /// Side by side. `width` is the dock's; `None` splits the region by
    /// the page's default ratio.
    Split { width: Option<f32> },
    /// The dock floats over the page's right edge.
    Overlay { width: f32 },
    /// The dock covers the page.
    Full,
}

/// A page's split with its dock (`usePanelSplit`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Split {
    /// The dock's width before the user resizes it; `None` keeps the
    /// page's ratio (the Messages board's 1.1fr / 1fr).
    pub default: Option<f32>,
    /// The dock's minimum width.
    pub min: f32,
}

impl Split {
    /// The companion dock (`PanelFrame`): 420px, at least 300.
    pub const COMPANION: Split = Split {
        default: Some(420.),
        min: 300.,
    };
    /// The Messages board's tab pane: ratio until resized, at least 316.
    pub const BOARD: Split = Split {
        default: None,
        min: 316.,
    };

    /// The dock's widest size beside a page `available` wide: the page
    /// keeps 320px.
    pub fn max(self, available: f32) -> f32 {
        self.min.max(available - 320.)
    }

    pub fn clamp(self, width: f32, available: f32) -> f32 {
        width.clamp(self.min, self.max(available))
    }

    /// Where the dock goes for a window `window` wide and a region
    /// `available` wide, with the user's `preferred` width.
    pub fn mode(self, window: f32, available: f32, preferred: Option<f32>) -> DockMode {
        if narrow(window) {
            return DockMode::Full;
        }
        let board_overlay = self.default.is_none() && available <= BOARD_OVERLAY;
        if window <= OVERLAY || board_overlay {
            return DockMode::Overlay {
                width: 420f32.min(available),
            };
        }
        DockMode::Split {
            width: preferred
                .or(self.default)
                .map(|width| self.clamp(width, available)),
        }
    }

    /// The width a split dock has now, for resizing from it.
    pub fn current(self, mode: DockMode, available: f32) -> f32 {
        match mode {
            DockMode::Split { width: Some(width) } => width,
            // The board's 1.1fr / 1fr split.
            DockMode::Split { width: None } => self.clamp(available / 2.1, available),
            DockMode::Overlay { width } => width,
            DockMode::Full => available,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_paddings_follow_the_breakpoints() {
        assert_eq!(header_padding(1200.), (16., 16.));
        assert_eq!(header_padding(700.), (8., 16.));
        assert_eq!(header_padding(639.), (8., 8.));
        assert_eq!(gutter(640.), 16.);
    }

    #[test]
    fn the_sidebar_stays_in_range_and_leaves_room_for_main() {
        assert_eq!(clamp_sidebar(260.4, 1200.), 260.);
        assert_eq!(clamp_sidebar(100., 1200.), 220.);
        assert_eq!(clamp_sidebar(900., 1200.), 520.);
        // 700 - 336 = 364.
        assert_eq!(clamp_sidebar(480., 700.), 364.);
        // Never below the minimum, even when main gets less.
        assert_eq!(clamp_sidebar(480., 400.), 220.);
    }

    #[test]
    fn header_pages_fold_when_they_would_overlap() {
        assert!(compact_pages(600., 10., 10., 10.));
        assert!(!compact_pages(1200., 140., 200., 180.));
        // 2 * (200 + 8) + 180 = 596.
        assert!(!compact_pages(700., 140., 200., 180.));
        assert!(compact_pages(595., 140., 200., 180.));
    }

    #[test]
    fn docks_split_overlay_or_fill() {
        let split = Split::COMPANION;
        assert_eq!(split.mode(640., 500., None), DockMode::Full);
        assert_eq!(
            split.mode(900., 600., None),
            DockMode::Overlay { width: 420. }
        );
        assert_eq!(
            split.mode(900., 300., None),
            DockMode::Overlay { width: 300. }
        );
        assert_eq!(
            split.mode(1400., 1000., None),
            DockMode::Split { width: Some(420.) }
        );
        // The page keeps 320px; the dock keeps its minimum.
        assert_eq!(
            split.mode(1400., 1000., Some(900.)),
            DockMode::Split { width: Some(680.) }
        );
        assert_eq!(
            split.mode(1400., 1000., Some(10.)),
            DockMode::Split { width: Some(300.) }
        );
    }

    #[test]
    fn the_board_keeps_its_ratio_until_resized() {
        let board = Split::BOARD;
        assert_eq!(
            board.mode(1400., 900., None),
            DockMode::Split { width: None }
        );
        assert_eq!(
            board.mode(1400., 900., Some(500.)),
            DockMode::Split { width: Some(500.) }
        );
        // A wide sidebar narrows the board below 651px.
        assert_eq!(
            board.mode(1100., 640., None),
            DockMode::Overlay { width: 420. }
        );
        let current = board.current(DockMode::Split { width: None }, 1050.);
        assert!((current - 500.).abs() < 0.01);
    }
}
