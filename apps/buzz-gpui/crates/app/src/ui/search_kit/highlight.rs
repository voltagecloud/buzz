//! The keyboard highlight of a search picker (`use-search-highlight.ts`).
//! Focus stays in the search field: Up and Down move the highlight and
//! Enter chooses the highlighted row. Typed text highlights the first row,
//! so Enter chooses it without an arrow key. The highlight follows a row's
//! key, not its position, as late results arrive: a row inserted above
//! cannot redirect Enter. When the highlighted row leaves, the highlight
//! returns to the first row rather than reviving the old one. It draws
//! nothing; rows keep their own markup.
//!
//! Each render: call [`Highlight::sync`] with the query and the row keys in
//! display order, read [`Highlight::active`], and call
//! [`Highlight::reveal`] with each row's child index in the list tracked by
//! [`Highlight::scroll_handle`].

use gpui_kit::{Pixels, Point, ScrollHandle};

#[derive(Clone, Debug)]
pub struct Highlight {
    /// The query the highlight was set for.
    query: String,
    /// The highlighted row, or `None`.
    key: Option<String>,
    /// The pointer set it; the row is already under the pointer, so the
    /// list stays still.
    pointer: bool,
    /// Counts keyboard moves, so each reveals its row again, even the same
    /// row after the list was scrolled away from it.
    moves: u64,
    /// The last pointer position over the list.
    last_pointer: Option<Point<Pixels>>,
    /// The row and move last scrolled into view.
    revealed: Option<(String, u64)>,
    highlight_empty: bool,
    wrap: bool,
    scroll: ScrollHandle,
}

impl Default for Highlight {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlight {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            key: None,
            pointer: false,
            moves: 0,
            last_pointer: None,
            revealed: None,
            highlight_empty: false,
            wrap: false,
            scroll: ScrollHandle::new(),
        }
    }

    /// Highlight the first row before anything is typed, for a picker whose
    /// unfiltered list is already the list to choose from.
    #[allow(dead_code)] // For pickers other than Command-K.
    pub fn highlight_empty(mut self, highlight_empty: bool) -> Self {
        self.highlight_empty = highlight_empty;
        self
    }

    /// Up from the first row goes to the last, and Down from the last row
    /// goes to the first. Command-K clamps instead.
    #[allow(dead_code)] // For pickers other than Command-K.
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Track this on the element whose direct children are the rows, so
    /// [`Highlight::reveal`] can scroll one into view.
    pub fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    fn fallback(&self, query: &str, keys: &[String]) -> Option<String> {
        (self.highlight_empty || !query.trim().is_empty())
            .then(|| keys.first().cloned())
            .flatten()
    }

    fn valid(&self, query: &str, keys: &[String]) -> bool {
        self.query == query && self.key.as_ref().is_some_and(|key| keys.contains(key))
    }

    /// Brings the highlight in line with the rows on show. A changed query
    /// or a highlighted row that left falls back to the first row (or none
    /// before anything is typed).
    pub fn sync(&mut self, query: &str, keys: &[String]) {
        if self.valid(query, keys) {
            return;
        }
        let fallback = self.fallback(query, keys);
        if self.query != query || self.key != fallback {
            self.query = query.to_owned();
            self.key = fallback;
            self.pointer = false;
            self.moves = 0;
        }
    }

    /// The highlighted row's key, after [`Highlight::sync`].
    pub fn active(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Highlights `key` from the keyboard, revealing it.
    pub fn select(&mut self, key: Option<String>) {
        self.key = key;
        self.pointer = false;
        self.moves += 1;
    }

    /// Up or Down: the next row, clamped at the ends unless the picker
    /// wraps. With nothing highlighted, Down goes to the first row and Up
    /// to the last. Returns false when there are no rows.
    pub fn step(&mut self, down: bool, keys: &[String]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let last = keys.len() - 1;
        let next = match self
            .key
            .as_ref()
            .and_then(|key| keys.iter().position(|k| k == key))
        {
            None if down => 0,
            None => last,
            Some(index) if self.wrap => {
                if down {
                    (index + 1) % keys.len()
                } else {
                    (index + last) % keys.len()
                }
            }
            Some(index) if down => (index + 1).min(last),
            Some(index) => index.saturating_sub(1),
        };
        self.select(Some(keys[next].clone()));
        true
    }

    /// The row Enter chooses: the highlighted one, if it is still listed.
    pub fn chosen(&self, keys: &[String]) -> Option<String> {
        self.key.clone().filter(|key| keys.contains(key))
    }

    /// The pointer moved over the list. Rows that move under a resting
    /// pointer do not count as a move.
    pub fn track_pointer(&mut self, position: Point<Pixels>) {
        self.last_pointer = Some(position);
    }

    /// The pointer moved over the row `key`. Only a real move highlights
    /// it, or new results arriving under a still pointer would steal
    /// Enter. Returns whether the highlight changed.
    pub fn hover(&mut self, key: &str, position: Point<Pixels>, keys: &[String]) -> bool {
        let moved = self.last_pointer.is_some_and(|last| last != position);
        self.last_pointer = Some(position);
        if !moved || self.key.as_deref() == Some(key) || !keys.iter().any(|k| k == key) {
            return false;
        }
        self.key = Some(key.to_owned());
        self.pointer = true;
        true
    }

    /// Scrolls the highlighted row into view after the keyboard moved it
    /// or the results changed it, but not after the pointer chose it.
    /// `index_of` gives a row's child index in the tracked element.
    pub fn reveal(&mut self, index_of: impl Fn(&str) -> Option<usize>) {
        let Some(key) = self.key.clone().filter(|_| !self.pointer) else {
            return;
        };
        let target = (key, self.moves);
        if self.revealed.as_ref() == Some(&target) {
            return;
        }
        if let Some(index) = index_of(&target.0) {
            self.scroll.scroll_to_item(index);
        }
        self.revealed = Some(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px};

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn nothing_is_highlighted_before_typing() {
        let rows = keys(&["a", "b"]);
        let mut highlight = Highlight::new();
        highlight.sync("", &rows);
        assert_eq!(highlight.active(), None);
        assert_eq!(highlight.chosen(&rows), None);
        highlight.sync(" ", &rows);
        assert_eq!(highlight.active(), None);
        highlight.sync("x", &rows);
        assert_eq!(highlight.active(), Some("a"));
        // Pickers whose list is the choice highlight it at once.
        let mut eager = Highlight::new().highlight_empty(true);
        eager.sync("", &rows);
        assert_eq!(eager.active(), Some("a"));
    }

    #[test]
    fn arrows_clamp_at_the_ends() {
        let rows = keys(&["a", "b", "c"]);
        let mut highlight = Highlight::new();
        highlight.sync("x", &rows);
        highlight.step(false, &rows);
        assert_eq!(highlight.active(), Some("a"));
        highlight.step(true, &rows);
        highlight.step(true, &rows);
        highlight.step(true, &rows);
        assert_eq!(highlight.active(), Some("c"));
        // With nothing highlighted, Up starts at the last row.
        let mut empty = Highlight::new();
        empty.sync("", &rows);
        empty.step(false, &rows);
        assert_eq!(empty.active(), Some("c"));
        assert!(!empty.step(true, &[]));
    }

    #[test]
    fn arrows_wrap_when_asked() {
        let rows = keys(&["a", "b"]);
        let mut highlight = Highlight::new().wrap(true);
        highlight.sync("x", &rows);
        highlight.step(false, &rows);
        assert_eq!(highlight.active(), Some("b"));
        highlight.step(true, &rows);
        assert_eq!(highlight.active(), Some("a"));
    }

    #[test]
    fn follows_the_row_key_as_results_arrive() {
        let mut highlight = Highlight::new();
        let rows = keys(&["a", "b"]);
        highlight.sync("x", &rows);
        highlight.step(true, &rows);
        assert_eq!(highlight.active(), Some("b"));
        // A row inserted above does not move the highlight.
        let later = keys(&["new", "a", "b"]);
        highlight.sync("x", &later);
        assert_eq!(highlight.chosen(&later).as_deref(), Some("b"));
        // The highlighted row leaving falls back to the first row.
        let gone = keys(&["new", "a"]);
        highlight.sync("x", &gone);
        assert_eq!(highlight.active(), Some("new"));
        // So does typing more, even when the old row is still there.
        highlight.step(true, &gone);
        highlight.sync("xy", &gone);
        assert_eq!(highlight.active(), Some("new"));
    }

    #[test]
    fn only_real_pointer_moves_highlight() {
        let rows = keys(&["a", "b"]);
        let mut highlight = Highlight::new();
        highlight.sync("x", &rows);
        let at = point(px(10.), px(10.));
        // The first event has nothing to compare with.
        assert!(!highlight.hover("b", at, &rows));
        // Rows moving under a resting pointer report the same position.
        assert!(!highlight.hover("b", at, &rows));
        assert_eq!(highlight.active(), Some("a"));
        assert!(highlight.hover("b", point(px(10.), px(12.)), &rows));
        assert_eq!(highlight.active(), Some("b"));
    }

    #[test]
    fn reveals_keyboard_moves_not_pointer_choices() {
        let rows = keys(&["a", "b", "c"]);
        let mut highlight = Highlight::new();
        highlight.sync("x", &rows);
        let index = |key: &str| rows.iter().position(|k| k == key);
        highlight.reveal(index);
        assert_eq!(highlight.revealed, Some(("a".into(), 0)));
        highlight.step(true, &rows);
        highlight.reveal(index);
        assert_eq!(highlight.revealed, Some(("b".into(), 1)));
        highlight.track_pointer(point(px(0.), px(0.)));
        highlight.hover("c", point(px(0.), px(5.)), &rows);
        highlight.reveal(index);
        assert_eq!(highlight.revealed, Some(("b".into(), 1)));
    }
}
