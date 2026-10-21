//! Concealed ranges: source bytes that are laid out without width, so a
//! field can hide markup (such as Markdown markers) while editing its
//! source. Like tokens they are presentation only: the text, its history and
//! the clipboard still see every byte. Coordinates are source bytes.
use std::ops::Range;

use gpui::{Context, SharedString};
use sum_tree::Bias;

use super::{InputBaseState, InputModeKind, cursor::CursorSelection};

/// A byte range of the text that is not drawn and takes no width. With a
/// replacement, that string is drawn in its place instead, in the style of
/// the range's first byte. The caret, selection, hit testing and deletion
/// treat the hidden bytes as invisible.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concealment {
    pub range: Range<usize>,
    pub replacement: Option<SharedString>,
}

impl Concealment {
    /// Hide `range` entirely.
    pub fn new(range: Range<usize>) -> Self {
        Self {
            range,
            replacement: None,
        }
    }
    /// Draw `replacement` in place of the hidden bytes.
    pub fn with_replacement(mut self, replacement: impl Into<SharedString>) -> Self {
        self.replacement = Some(replacement.into());
        self
    }
    /// Whether the range draws nothing, so both of its ends share one x.
    pub(super) fn is_zero_width(&self) -> bool {
        self.replacement.as_ref().is_none_or(|r| r.is_empty())
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    /// The ranges that apply now. Masked, secret and patterned inputs ignore
    /// concealment, since their display text is not the source.
    pub(super) fn active_concealments(&self) -> &[Concealment] {
        if M::CODE_EDITOR || self.masked || self.token_is_secret() || !self.mask_pattern.is_none() {
            &[]
        } else {
            &self.concealed_ranges
        }
    }

    pub(super) fn concealment_visible(&self) -> bool {
        !self.active_concealments().is_empty()
    }

    /// Whether some line needs the inline-fragment layout: it holds a token
    /// or a concealed range.
    pub(super) fn inline_items_visible(&self) -> bool {
        self.tokens_visible() || self.concealment_visible()
    }

    /// Move an offset strictly inside a concealed range to its start (left
    /// bias) or end (right bias).
    pub(super) fn conceal_boundary(&self, offset: usize, bias: Bias) -> usize {
        let ranges = self.active_concealments();
        let ix = ranges.partition_point(|c| c.range.end <= offset);
        match ranges.get(ix).filter(|c| c.range.start < offset) {
            Some(c) if bias == Bias::Left => c.range.start,
            Some(c) => c.range.end,
            None => offset,
        }
    }

    /// Whether `range` lies wholly inside one concealed range.
    pub(super) fn is_concealed(&self, range: Range<usize>) -> bool {
        let ranges = self.active_concealments();
        let ix = ranges.partition_point(|c| c.range.end < range.end);
        ranges
            .get(ix)
            .is_some_and(|c| c.range.start <= range.start && range.end <= c.range.end)
    }

    /// Past every concealed range starting at `offset`.
    pub(super) fn skip_concealed_forward(&self, mut offset: usize) -> usize {
        let ranges = self.active_concealments();
        let mut ix = ranges.partition_point(|c| c.range.start < offset);
        while let Some(c) = ranges.get(ix).filter(|c| c.range.start == offset) {
            offset = c.range.end;
            ix += 1;
        }
        offset
    }

    /// Before every concealed range ending at `offset`.
    pub(super) fn skip_concealed_backward(&self, mut offset: usize) -> usize {
        let ranges = self.active_concealments();
        let mut ix = ranges.partition_point(|c| c.range.end < offset);
        while ix < ranges.len() && ranges[ix].range.end == offset {
            offset = ranges[ix].range.start;
            let Some(prev) = ix.checked_sub(1) else {
                break;
            };
            ix = prev;
        }
        offset
    }

    /// The leftmost offset drawn at the same x as `offset`: hit testing
    /// cannot tell the ends of a zero-width range apart, so take the one
    /// before the hidden bytes.
    pub(super) fn leftmost_visual_offset(&self, mut offset: usize) -> usize {
        let ranges = self.active_concealments();
        let mut ix = ranges.partition_point(|c| c.range.end < offset);
        while ix < ranges.len() && ranges[ix].range.end == offset && ranges[ix].is_zero_width() {
            offset = ranges[ix].range.start;
            let Some(prev) = ix.checked_sub(1) else {
                break;
            };
            ix = prev;
        }
        offset
    }

    /// Clamp a selection out of concealed ranges, as tokens do: a caret
    /// moves past the hidden bytes, a selection grows over them. Tokens are
    /// normalized afterwards.
    pub(super) fn normalize_selection_range(&self, range: Range<usize>) -> Range<usize> {
        let range = if range.is_empty() {
            let offset = self.conceal_boundary(range.start, Bias::Right);
            offset..offset
        } else {
            self.conceal_boundary(range.start, Bias::Left)
                ..self.conceal_boundary(range.end, Bias::Right)
        };
        self.normalize_token_range(range)
    }

    fn clamp_selections_out_of_concealment(&mut self) {
        if self.ime_marked_range.is_some() {
            return;
        }
        let selections: Vec<CursorSelection> = self
            .selections
            .iter()
            .map(|sel| {
                let range = self.normalize_selection_range(sel.start..sel.end);
                let mut sel = *sel;
                sel.start = range.start;
                sel.end = range.end;
                sel
            })
            .collect();
        self.selections.replace_all(selections);
        self.selections.merge_overlapping();
    }

    /// Follow an edit of `range` (old coordinates) by text `new_len` bytes
    /// long: ranges after it shift and ranges it touches inside go away. A
    /// range ending where text is inserted stays; one starting there shifts.
    pub(super) fn adjust_concealments(&mut self, range: &Range<usize>, new_len: usize) {
        if self.concealed_ranges.is_empty() {
            return;
        }
        let shift = new_len as isize - range.len() as isize;
        let before = self.concealed_ranges.len();
        let mut changed = false;
        self.concealed_ranges.retain_mut(|c| {
            if c.range.start < range.end && range.start < c.range.end {
                return false;
            }
            if c.range.start >= range.end && shift != 0 {
                changed = true;
                c.range = c.range.start.saturating_add_signed(shift)
                    ..c.range.end.saturating_add_signed(shift);
            }
            true
        });
        // Undo and normalization can change text the edit range does not
        // describe, so never trust a shifted range blindly.
        let text = &self.text;
        self.concealed_ranges.retain(|c| {
            c.range.start < c.range.end
                && c.range.end <= text.len()
                && text.is_char_boundary(c.range.start)
                && text.is_char_boundary(c.range.end)
        });
        if changed || self.concealed_ranges.len() != before {
            self.concealed_revision = self.concealed_revision.wrapping_add(1);
        }
    }

    /// Sort, and drop what cannot be concealed: empty, out-of-bounds or
    /// split-character ranges, ranges crossing a line break, ranges
    /// overlapping an earlier one or a token.
    fn sanitize_concealments(&self, mut ranges: Vec<Concealment>) -> Vec<Concealment> {
        if M::CODE_EDITOR {
            return Vec::new();
        }
        ranges.sort_by_key(|c| (c.range.start, c.range.end));
        let text = &self.text;
        let tokens = self.token_spans();
        let mut out: Vec<Concealment> = Vec::with_capacity(ranges.len());
        for mut c in ranges {
            let Range { start, end } = c.range;
            if start >= end
                || end > text.len()
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
                || out.last().is_some_and(|last| last.range.end > start)
                || text
                    .slice(start..end)
                    .chars()
                    .any(|ch| matches!(ch, '\n' | '\r'))
            {
                continue;
            }
            let ix = tokens.partition_point(|s| s.range().end <= start);
            if tokens.get(ix).is_some_and(|s| s.range().start < end) {
                continue;
            }
            if c.replacement.as_ref().is_some_and(|r| r.is_empty()) {
                c.replacement = None;
            }
            out.push(c);
        }
        out
    }

    pub(super) fn replace_concealments(
        &mut self,
        ranges: Vec<Concealment>,
        cx: &mut Context<Self>,
    ) {
        let ranges = self.sanitize_concealments(ranges);
        if ranges == self.concealed_ranges {
            return;
        }
        self.concealed_ranges = ranges;
        self.concealed_revision = self.concealed_revision.wrapping_add(1);
        self.clamp_selections_out_of_concealment();
        cx.notify();
    }
}

macro_rules! concealment_api {
    ($mode:ty) => {
        impl InputBaseState<$mode> {
            /// Replace the concealed ranges. They are sorted, and ranges that
            /// are empty, off character boundaries, cross a line break,
            /// overlap an earlier range or overlap a token are dropped.
            ///
            /// This changes only how the text is shown: the value, history
            /// and copied text are untouched and no change event is
            /// emitted. A caret inside a new range moves to its end.
            /// Edits shift the ranges after them and drop the ones they
            /// touch inside, so set them again after every change. Masked
            /// inputs keep the ranges but ignore them while masked.
            pub fn set_concealed_ranges(
                &mut self,
                ranges: Vec<Concealment>,
                cx: &mut Context<Self>,
            ) {
                self.replace_concealments(ranges, cx);
            }
            /// The concealed ranges, sorted and disjoint.
            pub fn concealed_ranges(&self) -> &[Concealment] {
                &self.concealed_ranges
            }
        }
    };
}
concealment_api!(super::InputMode);
concealment_api!(super::TextareaMode);

#[cfg(test)]
mod tests {
    use super::Concealment;
    use crate::actions::{SelectLeft, SelectRight};
    use crate::input::{
        Backspace, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, InlineToken,
        InputBaseState, InputContent, InputEvent, InputModeKind, InputState, MoveDown, MoveLeft,
        MoveRight, MoveToNextWord, MoveToPreviousWord, Redo, TextareaState, Undo,
    };
    use gpui::{
        AppContext as _, Context, Entity, EntityInputHandler as _, HighlightStyle, IntoElement,
        ParentElement as _, Pixels, Render, Styled as _, TestAppContext, VisualTestContext, Window,
        div, point, px, size,
    };
    use std::{cell::Cell, ops::Range, rc::Rc};

    struct Harness<M: InputModeKind>(Entity<InputBaseState<M>>);

    impl<M: InputModeKind> Render for Harness<M> {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    fn open<M: InputModeKind>(
        cx: &mut TestAppContext,
        width: f32,
        make: impl FnOnce(&mut Window, &mut Context<InputBaseState<M>>) -> InputBaseState<M>,
    ) -> (Entity<InputBaseState<M>>, VisualTestContext) {
        cx.update(crate::init);
        let mut state = None;
        let window = cx.open_window(size(px(width), px(200.)), |window, cx| {
            let entity = cx.new(|cx| make(window, cx));
            state = Some(entity.clone());
            Harness(entity)
        });
        let cx = VisualTestContext::from_window(window.into(), cx);
        (state.unwrap(), cx)
    }

    fn textarea(
        cx: &mut TestAppContext,
        text: &'static str,
    ) -> (Entity<TextareaState>, VisualTestContext) {
        open(cx, 400., move |window, cx| {
            TextareaState::new(window, cx).default_value(text)
        })
    }

    fn hide(ranges: &[Range<usize>]) -> Vec<Concealment> {
        ranges.iter().cloned().map(Concealment::new).collect()
    }

    fn ranges<M: InputModeKind>(state: &InputBaseState<M>) -> Vec<Range<usize>> {
        state
            .concealed_ranges
            .iter()
            .map(|c| c.range.clone())
            .collect()
    }

    /// X of each offset on the first visual row of the first line.
    fn xs<M: InputModeKind>(state: &InputBaseState<M>, offsets: &[usize]) -> Vec<Pixels> {
        let line = &state.last_layout.as_ref().unwrap().lines[0].wrapped_lines[0];
        offsets.iter().map(|&ix| line.x_for_index(ix)).collect()
    }

    #[gpui::test]
    fn setting_ranges_sanitizes_without_editing(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**\nü*x*");
        let changes = Rc::new(Cell::new(0));
        let counter = changes.clone();
        cx.update(|_, cx| {
            cx.subscribe(&state, move |_, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) {
                    counter.set(counter.get() + 1);
                }
            })
            .detach();
        });
        cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(
                    vec![
                        Concealment::new(4..6),
                        Concealment::new(1..3),
                        // Overlaps 1..3.
                        Concealment::new(2..4),
                        Concealment::new(3..3),
                        // Crosses the line break.
                        Concealment::new(5..8),
                        // Splits "ü".
                        Concealment::new(8..9),
                        Concealment::new(9..10).with_replacement(""),
                        Concealment::new(11..99),
                    ],
                    cx,
                );
                assert_eq!(ranges(state), vec![1..3, 4..6, 9..10]);
                assert_eq!(state.concealed_ranges()[2].replacement, None);
                assert_eq!(state.value().as_ref(), "a**b**\nü*x*");
                assert!(!state.undo_manager.has_undos());
            });
        });
        assert_eq!(changes.get(), 0);
    }

    #[gpui::test]
    fn concealed_ranges_take_no_width_or_their_replacement(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**\nplain");
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let plain = xs(state.read(cx), &[1, 3, 4, 6]);
            assert!(plain[1] > plain[0], "markers have width when shown");
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx)
            });
            window.draw(cx).clear(cx);
            let hidden = xs(state.read(cx), &[1, 3, 4, 6]);
            assert_eq!(hidden[0], hidden[1]);
            assert_eq!(hidden[2], hidden[3]);
            assert_eq!(
                hidden[2] - hidden[1],
                plain[2] - plain[1],
                "b keeps its width"
            );
            let layout = state.read(cx).last_layout.as_ref().unwrap();
            assert_eq!(layout.lines[0].wrapped_lines[0].width, hidden[3]);
            assert!(layout.lines[0].wrapped_lines[0].is_inline());
            assert!(
                !layout.lines[1].wrapped_lines[0].is_inline(),
                "a row without concealment keeps the plain path"
            );

            state.update(cx, |state, cx| {
                state.set_concealed_ranges(
                    vec![
                        Concealment::new(1..3).with_replacement("•"),
                        Concealment::new(4..6),
                    ],
                    cx,
                )
            });
            window.draw(cx).clear(cx);
            let replaced = xs(state.read(cx), &[1, 3, 4, 6]);
            assert!(replaced[1] > replaced[0], "the replacement takes width");
            assert!(replaced[1] - replaced[0] < plain[1] - plain[0]);
            assert_eq!(replaced[2], replaced[3]);
        });
    }

    #[gpui::test]
    fn concealed_ranges_shrink_soft_wrapping(cx: &mut TestAppContext) {
        let text = "aaaa XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX bbbb";
        let (state, mut cx) = open(cx, 240., move |window, cx| {
            TextareaState::new(window, cx).default_value(text)
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let wrapped = state.read(cx).display_map.wrap_row_count();
            assert!(wrapped > 1, "the plain text wraps");
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[5..69]), cx)
            });
            window.draw(cx).clear(cx);
            assert_eq!(state.read(cx).display_map.wrap_row_count(), 1);
            state.update(cx, |state, cx| state.set_concealed_ranges(vec![], cx));
            window.draw(cx).clear(cx);
            assert_eq!(state.read(cx).display_map.wrap_row_count(), wrapped);
        });
    }

    #[gpui::test]
    fn concealed_ranges_shrink_unwrapped_width(cx: &mut TestAppContext) {
        let text = "aaaa XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX bbbb";
        let (state, mut cx) = open(cx, 240., move |window, cx| {
            InputState::new(window, cx).default_value(text)
        });
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[5..37]), cx)
            });
            window.draw(cx).clear(cx);
            let state = state.read(cx);
            let unwrapped = state.token_layout_cache.as_ref().unwrap().unwrapped_width;
            let line = &state.last_layout.as_ref().unwrap().lines[0].wrapped_lines[0];
            assert_eq!(line.x_for_index(5), line.x_for_index(37));
            assert!((unwrapped - line.width).abs() < px(2.));
        });
    }

    #[gpui::test]
    fn arrows_step_over_concealed_ranges(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx);
                let mut step = |from: usize, right: bool| {
                    state.set_selected_range(from..from, cx);
                    if right {
                        state.right(&MoveRight, window, cx);
                    } else {
                        state.left(&MoveLeft, window, cx);
                    }
                    state.cursor()
                };
                assert_eq!(step(0, true), 1);
                assert_eq!(step(1, true), 4);
                assert_eq!(step(4, true), 6);
                assert_eq!(step(6, false), 3);
                assert_eq!(step(3, false), 0);
                assert_eq!(step(4, false), 3);
                // Never inside a range.
                state.set_selected_range(2..2, cx);
                assert_eq!(state.cursor(), 3);
            });
        });
    }

    #[gpui::test]
    fn shift_arrows_extend_over_concealed_ranges(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx);
                state.set_selected_range(0..0, cx);
                state.select_right(&SelectRight, window, cx);
                assert_eq!(state.selected_range(), 0..1);
                state.select_right(&SelectRight, window, cx);
                assert_eq!(state.selected_range(), 0..4);
                state.select_right(&SelectRight, window, cx);
                assert_eq!(state.selected_range(), 0..6);
                state.select_left(&SelectLeft, window, cx);
                assert_eq!(state.selected_range(), 0..3);
                state.select_left(&SelectLeft, window, cx);
                assert_eq!(state.selected_range(), 0..0);
                // A selection grows over a range it ends inside.
                state.set_selected_range(0..2, cx);
                assert_eq!(state.selected_range(), 0..3);
            });
        });
    }

    #[gpui::test]
    fn word_moves_skip_hidden_markers(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "foo **bar** baz");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[4..6, 9..11]), cx);
                let mut word = |from: usize, right: bool| {
                    state.set_selected_range(from..from, cx);
                    if right {
                        state.move_to_next_word(&MoveToNextWord, window, cx);
                    } else {
                        state.move_to_previous_word(&MoveToPreviousWord, window, cx);
                    }
                    state.cursor()
                };
                assert_eq!(word(3, true), 9);
                assert_eq!(word(9, true), 15);
                assert_eq!(word(15, false), 12);
                assert_eq!(word(12, false), 6);
                assert_eq!(word(6, false), 0);
            });
        });
    }

    #[gpui::test]
    fn clicks_resolve_to_the_leftmost_offset(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**\nc");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx)
            });
            window.draw(cx).clear(cx);
            let state = state.read(cx);
            let bounds = state.last_bounds.unwrap();
            let layout = state.last_layout.as_ref().unwrap();
            let y = layout.line_height / 2.;
            let origin = bounds.origin + point(layout.line_number_width, px(0.));
            let x_b = xs(state, &[3])[0];
            let click = |x: Pixels| state.index_for_mouse_position(origin + point(x, y)).0;
            assert_eq!(click(x_b + px(0.5)), 1, "between a and b");
            assert_eq!(click(px(1000.)), 4, "past the end of the row");
            assert_eq!(click(px(0.)), 0);
        });
    }

    #[gpui::test]
    fn vertical_moves_take_the_leftmost_offset(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "ab\na**b**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[4..6, 7..9]), cx)
            });
            window.draw(cx).clear(cx);
            state.update(cx, |state, cx| {
                state.set_selected_range(1..1, cx);
                state.down(&MoveDown, window, cx);
                assert_eq!(state.cursor(), 4, "after a, before the hidden markers");
            });
        });
    }

    #[gpui::test]
    fn backspace_and_delete_remove_visible_text(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "**bold**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[0..2, 6..8]), cx);
                state.set_selected_range(8..8, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value().as_ref(), "**bol**");
                assert_eq!(state.cursor(), 5);

                state.set_value("**bold**", window, cx);
                state.set_concealed_ranges(hide(&[0..2, 6..8]), cx);
                state.set_selected_range(0..0, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value().as_ref(), "**old**");
                assert_eq!(state.cursor(), 2);

                // Only hidden text before the caret: nothing to delete.
                state.set_value("**bold**", window, cx);
                state.set_concealed_ranges(hide(&[0..2, 6..8]), cx);
                state.set_selected_range(2..2, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value().as_ref(), "**bold**");
                // Only hidden text after it.
                state.set_selected_range(6..6, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value().as_ref(), "**bold**");

                // A selection deletes exactly its bytes, hidden ones too.
                state.set_selected_range(0..6, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value().as_ref(), "**");
            });
        });
    }

    #[gpui::test]
    fn word_deletion_keeps_hidden_markers(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "foo **bar** baz");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[4..6, 9..11]), cx);
                state.set_selected_range(11..11, cx);
                state.delete_previous_word(&DeleteToPreviousWordStart, window, cx);
                assert_eq!(state.value().as_ref(), "foo **** baz");

                state.set_value("foo **bar** baz", window, cx);
                state.set_concealed_ranges(hide(&[4..6, 9..11]), cx);
                state.set_selected_range(4..4, cx);
                state.delete_next_word(&DeleteToNextWordEnd, window, cx);
                assert_eq!(state.value().as_ref(), "foo **** baz");
            });
        });
    }

    #[gpui::test]
    fn edits_shift_or_drop_ranges(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**c");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx);
                // Inserting where a range ends keeps it; one starting there shifts.
                state.replace_text_in_range(Some(3..3), "X", window, cx);
                assert_eq!(ranges(state), vec![1..3, 5..7]);
                state.replace_text_in_range(Some(1..1), "Y", window, cx);
                assert_eq!(ranges(state), vec![2..4, 6..8]);
                // An edit inside a range or over part of it drops it.
                state.replace_text_in_range(Some(3..3), "Z", window, cx);
                assert_eq!(ranges(state), vec![7..9]);
                state.replace_text_in_range(Some(6..8), "", window, cx);
                assert!(ranges(state).is_empty());
            });
        });
    }

    #[gpui::test]
    fn undo_and_redo_keep_ranges_inside_the_text(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "é**ü**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let len = state.text.len();
                state.set_concealed_ranges(hide(&[2..4, 6..8]), cx);
                state.set_selected_range(len..len, cx);
                state.replace_text_in_range(None, "ñ", window, cx);
                state.set_concealed_ranges(hide(&[2..4, 6..8]), cx);
                state.set_selected_range(0..len + 2, cx);
                state.replace_text_in_range(None, "", window, cx);
                for undo in [true, true, false, false, true] {
                    if undo {
                        state.undo(&Undo, window, cx);
                    } else {
                        state.redo(&Redo, window, cx);
                    }
                    let text = state.value();
                    let mut end = 0;
                    for c in state.concealed_ranges() {
                        assert!(end <= c.range.start && c.range.start < c.range.end);
                        assert!(c.range.end <= text.len());
                        assert!(text.is_char_boundary(c.range.start));
                        assert!(text.is_char_boundary(c.range.end));
                        end = c.range.end;
                    }
                    let cursor = state.cursor();
                    assert!(text.is_char_boundary(cursor));
                    assert_eq!(state.conceal_boundary(cursor, sum_tree::Bias::Left), cursor);
                }
            });
        });
    }

    #[gpui::test]
    fn ranges_over_tokens_are_ignored(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                let content = InputContent::new("@a**b**")
                    .with_token(0..2, InlineToken::new("a", "@a"))
                    .unwrap();
                state.set_value(content, window, cx);
                state.set_concealed_ranges(hide(&[1..3, 2..4, 5..7]), cx);
                assert_eq!(ranges(state), vec![2..4, 5..7]);
            });
            window.draw(cx).clear(cx);
            let state = state.read(cx);
            assert!(state.tokens_visible());
            let x = xs(state, &[2, 4, 5, 7]);
            assert_eq!(x[0], x[1]);
            assert_eq!(x[2], x[3]);
        });
    }

    #[gpui::test]
    fn concealed_lines_paint_decoration_backgrounds(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "`code` x");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[0..1, 5..6]), cx);
                let fill = HighlightStyle {
                    background_color: Some(gpui::red()),
                    ..Default::default()
                };
                state.create_decorations_collection(
                    vec![crate::input::TextDecoration::new(0..6, fill)],
                    cx,
                );
            });
            window.draw(cx).clear(cx);
            let layout = state.read(cx).last_layout.as_ref().unwrap();
            assert!(layout.lines[0].wrapped_lines[0].is_inline());
            assert!(layout.lines[0].has_background());
        });
    }

    #[gpui::test]
    fn ime_composition_keeps_ranges_sound(cx: &mut TestAppContext) {
        let (state, mut cx) = textarea(cx, "a**b**");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx);
                state.set_selected_range(4..4, cx);
                state.replace_and_mark_text_in_range(None, "中", None, window, cx);
                assert_eq!(ranges(state), vec![1..3, 7..9]);
                // Setting ranges mid-composition leaves the marked text be.
                state.set_concealed_ranges(hide(&[1..3, 7..9]), cx);
                assert_eq!(state.selected_range(), 7..7);
                state.replace_text_in_range(None, "中文", window, cx);
                assert_eq!(state.value().as_ref(), "a**b中文**");
                assert_eq!(ranges(state), vec![1..3, 10..12]);
            });
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    fn masked_inputs_ignore_concealment(cx: &mut TestAppContext) {
        let (input, mut cx) = open(cx, 240., |window, cx| {
            InputState::new(window, cx).default_value("a**b**")
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_concealed_ranges(hide(&[1..3, 4..6]), cx);
                state.set_masked(true, window, cx);
                assert!(!state.inline_items_visible());
                assert_eq!(state.previous_boundary(4), 3);
                assert_eq!(state.next_boundary(1), 2);
            });
            window.draw(cx).clear(cx);
            input.update(cx, |state, cx| state.set_masked(false, window, cx));
            window.draw(cx).clear(cx);
            let x = xs(input.read(cx), &[1, 3]);
            assert_eq!(x[0], x[1]);
        });
    }
}
