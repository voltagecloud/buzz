//! The pieces every search picker shares (`features/search`): name matching
//! with the matched letters underlined, ranking by where the viewer goes,
//! and the keyboard highlight. Command-K uses all three; other pickers,
//! such as a channel tab picker, reuse them so they rank and behave alike.
//!
//! - [`match_name`] ranks a label against typed text and reports the
//!   matched code points; [`matched_label`] draws them underlined.
//! - [`usage`] remembers visits and choices per community and viewer, and
//!   [`usage::search_order`] turns a match rank into the shared order.
//! - [`Highlight`] is the keyed keyboard highlight.

mod highlight;
pub mod usage;

// Part of the API for every picker, even those Command-K does not use.
#[allow(unused_imports)]
pub use buzz_relay_client::search::{NameMatch, match_name, match_rank};
pub use highlight::Highlight;
#[allow(unused_imports)]
pub use usage::{SearchUsage, picker_text, record_choice, record_visit, search_order};

use gpui_kit::*;
use std::ops::Range;

/// A label with its matched letters underlined (`MatchedLabel`). The text
/// stays whole, so it truncates and reads as one label. `positions` are
/// code points, as [`match_name`] reports them.
pub fn matched_label(label: impl Into<SharedString>, positions: &[usize]) -> StyledText {
    let label = label.into();
    let underline = HighlightStyle {
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        ..Default::default()
    };
    let runs = matched_runs(&label, positions);
    StyledText::new(label).with_highlights(runs.into_iter().map(|run| (run, underline)))
}

/// The byte ranges of `label` covered by `positions`, joined into runs.
/// Positions past the end are ignored.
pub fn matched_runs(label: &str, positions: &[usize]) -> Vec<Range<usize>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (index, (at, c)) in label.char_indices().enumerate() {
        if !positions.contains(&index) {
            continue;
        }
        let end = at + c.len_utf8();
        match runs.last_mut() {
            Some(run) if run.end == at => run.end = end,
            _ => runs.push(at..end),
        }
    }
    runs
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // Runs, not a range of values.
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn joins_matched_letters_into_byte_runs() {
        assert_eq!(matched_runs("general", &[0, 1, 2]), [0..3]);
        assert_eq!(
            matched_runs("buzz-github-prs", &[0, 5, 12]),
            [0..1, 5..6, 12..13]
        );
        // Code points, not bytes.
        assert_eq!(matched_runs("équipe", &[0, 1]), [0..3]);
        assert!(matched_runs("abc", &[]).is_empty());
        assert!(matched_runs("abc", &[7]).is_empty());
    }

    #[test]
    fn underlines_what_match_name_found() {
        let found = match_name("team-general", "gen").expect("matches");
        assert_eq!(matched_runs("team-general", &found.positions), [5..8]);
    }
}
