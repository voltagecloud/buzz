//! Markdown styled as it is typed (`EditableInput.tsx`,
//! `composer-inline-input.ts`, `EditableInput.module.css`).
//!
//! buzz-app's composer is a rich-text editor: a closed `**bold**` turns
//! into bold text and its markers disappear. This composer edits the
//! Markdown source instead, and gets the same look by hiding the syntax:
//! the text between markers takes the style, and the markers are
//! concealed ranges of the text field (third_party/README.md), which the
//! caret steps over as if they were not there. A bullet shows as a dot and
//! a quote's `>` as a bar. The scanner runs on every edit, over the field's
//! text only, in one pass that keeps its buffers between edits. It follows
//! CommonMark and GFM closely enough for chat; anything it misreads only
//! looks different here and still sends as typed.

use buzz_ui::{ActiveTheme, Colors};
use gpui_base::input::Concealment;
use gpui_kit::component::input::{
    TextDecoration, TextDecorationCollection, TextareaMode, TextareaState,
};
use gpui_kit::{App, Entity, FontStyle, FontWeight, HighlightStyle, Hsla};
use gpui_kit::{StrikethroughStyle, UnderlineStyle, px};
use std::ops::Range;

/// What a run of text is. Spans nest, so each kind is a bit.
pub type Kinds = u16;
pub const STRONG: Kinds = 1;
pub const EMPHASIS: Kinds = 1 << 1;
pub const STRIKE: Kinds = 1 << 2;
/// A code span or the lines of a fenced code block.
pub const CODE: Kinds = 1 << 3;
pub const LINK: Kinds = 1 << 4;
pub const SPOILER: Kinds = 1 << 5;
/// Inline syntax: delimiters, backticks, brackets, link destinations,
/// escapes, fences and rules.
const MARKER: Kinds = 1 << 6;
/// A quote's `>` or a list item's bullet, number or task box.
const BLOCK: Kinds = 1 << 7;
/// The text of a quote.
const QUOTE: Kinds = 1 << 8;
/// A chip: never styled.
const CHIP: Kinds = 1 << 9;
/// A thematic break, which stays as typed.
const RULE: Kinds = 1 << 10;
const KINDS: usize = 11;

/// What the toolbar shows as pressed at the caret.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Active {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub spoiler: bool,
    pub link: bool,
}

const NONE: usize = usize::MAX;

/// The composer's styled ranges, kept as decorations on its text field.
pub struct LiveMarkdown {
    collection: TextDecorationCollection<TextareaMode>,
    scanner: Scanner,
    /// The text, with chips blanked out.
    bytes: Vec<u8>,
    /// The styled runs of the last scan, for the toolbar's state.
    runs: Vec<(Range<usize>, Kinds)>,
    /// The pending delimiters of the last scan.
    last_pending: Vec<Range<usize>>,
    /// The colours of the theme the composer last rendered in, which may
    /// be a scope's (the media viewer's comments are always dark).
    palette: Option<Palette>,
}

impl LiveMarkdown {
    pub fn new(input: &Entity<TextareaState>, cx: &mut App) -> Self {
        let collection = input.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        Self {
            collection,
            scanner: Scanner::default(),
            bytes: Vec::new(),
            runs: Vec::new(),
            last_pending: Vec::new(),
            palette: None,
        }
    }

    /// Styles the text again, after it changed. `pending` are the empty
    /// delimiters a collapsed-caret toggle left around the caret, which
    /// stay hidden until text fills them.
    pub fn restyle(
        &mut self,
        input: &Entity<TextareaState>,
        pending: &[Range<usize>],
        cx: &mut App,
    ) {
        let palette = *self
            .palette
            .get_or_insert_with(|| Palette::new(cx.colors()));
        let state = input.read(cx);
        self.bytes.clear();
        for chunk in state.text().chunks() {
            self.bytes.extend_from_slice(chunk.as_bytes());
        }
        // A chip's source is one opaque word: Markdown inside it, like a
        // channel reference's link, is not the writer's.
        for span in state.tokens() {
            self.bytes[span.range()].fill(b'x');
        }
        let Ok(text) = std::str::from_utf8(&self.bytes) else {
            return;
        };
        let mut decorations = Vec::new();
        let mut runs = std::mem::take(&mut self.runs);
        runs.clear();
        self.scanner.scan(
            text,
            state.tokens().iter().map(|span| span.range()),
            |range, kinds| {
                runs.push((range.clone(), kinds));
                decorations.push(TextDecoration::new(range, palette.style(kinds)))
            },
        );
        self.runs = runs;
        let concealed = self.scanner.concealments(text, pending);
        self.last_pending = pending.to_vec();
        self.collection.set(decorations, cx);
        input.update(cx, |state, cx| state.set_concealed_ranges(concealed, cx));
    }

    /// The matched spans of `kind` in `text`, for tests.
    #[cfg(test)]
    pub fn scan_pairs(text: &str, kind: Kinds) -> Vec<Pair> {
        let mut scanner = Scanner::default();
        scanner.scan(text, None, |_, _| {});
        scanner
            .pairs
            .into_iter()
            .filter(|pair| pair.kind & kind != 0)
            .collect()
    }

    /// The bare URLs of the text, which Mod+K edits as links.
    pub fn bare_links(&self) -> Vec<Range<usize>> {
        self.scanner.bare.clone()
    }

    /// The matched spans of `kind`, by their markers.
    pub fn pairs(&self, kind: Kinds) -> Vec<Pair> {
        self.scanner
            .pairs
            .iter()
            .filter(|pair| pair.kind & kind != 0)
            .cloned()
            .collect()
    }

    /// Whether any of `range` is code, where a typed Space or closing
    /// colon stays literal.
    pub fn in_code(&self, range: Range<usize>) -> bool {
        self.runs
            .iter()
            .any(|(run, kinds)| kinds & CODE != 0 && run.start < range.end && range.start < run.end)
    }

    /// The formats at the caret or over all of a selection. `pending`
    /// formats wait at the caret.
    pub fn active(&self, selection: Range<usize>, text: &str, pending: Kinds) -> Active {
        active_in(&self.runs, selection, text, pending)
    }
}

/// [`LiveMarkdown::active`] over the styled runs of a scan.
fn active_in(
    runs: &[(Range<usize>, Kinds)],
    selection: Range<usize>,
    text: &str,
    pending: Kinds,
) -> Active {
    let mut kinds: Kinds = pending;
    if selection.is_empty() {
        // The formats of the character before the caret, as a mark's.
        let at = selection.start;
        for (range, k) in runs {
            if range.start < at && at <= range.end && k & (MARKER | BLOCK | RULE) == 0 {
                kinds |= k;
            }
        }
    } else {
        let mut all: Option<Kinds> = None;
        let mut covered = selection.start;
        for (range, k) in runs {
            if range.end <= selection.start || range.start >= selection.end {
                continue;
            }
            if range.start > covered && !text[covered..range.start].trim().is_empty() {
                all = Some(0);
            }
            // Hidden syntax is no text of its own.
            if k & (MARKER | BLOCK | RULE) != 0 {
                covered = covered.max(range.end);
                continue;
            }
            all = Some(all.map_or(*k, |all| all & k));
            covered = covered.max(range.end);
        }
        if covered < selection.end && !text[covered..selection.end].trim().is_empty() {
            all = Some(0);
        }
        kinds |= all.unwrap_or(0);
    }
    Active {
        bold: kinds & STRONG != 0,
        italic: kinds & EMPHASIS != 0,
        strike: kinds & STRIKE != 0,
        code: kinds & CODE != 0,
        spoiler: kinds & SPOILER != 0,
        link: kinds & LINK != 0,
    }
}

impl LiveMarkdown {
    /// Restyles when the composer renders in other colours than before.
    pub fn follow_theme(&mut self, input: &Entity<TextareaState>, colors: &Colors, cx: &mut App) {
        let palette = Some(Palette::new(colors));
        if self.palette != palette {
            self.palette = palette;
            let pending = std::mem::take(&mut self.last_pending);
            self.restyle(input, &pending, cx);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Palette {
    marker: Hsla,
    code: Hsla,
    code_fill: Hsla,
    subtle: Hsla,
    spoiler_fill: Hsla,
}

impl Palette {
    fn new(c: &Colors) -> Self {
        Self {
            // The quote bar's colour (ui.md §4.6): dim but legible in both themes.
            marker: c.border_prominent,
            code: c.text_accent,
            code_fill: c.affordance_selected,
            subtle: c.text_subtle,
            spoiler_fill: c.affordance_subtle,
        }
    }

    /// The timeline's styles (`markdown::styled_spans`, ui.md §4.6), with
    /// what the body font can't show standing in: code takes a colour and a
    /// stronger fill than its mono face would need, and a quote's text is
    /// subtle in place of the composer's bar.
    fn style(&self, kinds: Kinds) -> HighlightStyle {
        let has = |kind| kinds & kind != 0;
        let color = if has(BLOCK) {
            Some(self.subtle)
        } else if has(MARKER | RULE) {
            Some(self.marker)
        } else if has(CODE) {
            Some(self.code)
        } else {
            None
        };
        HighlightStyle {
            color,
            font_weight: has(STRONG).then_some(FontWeight::MEDIUM),
            font_style: has(EMPHASIS).then_some(FontStyle::Italic),
            strikethrough: has(STRIKE).then_some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            }),
            background_color: if has(CODE) {
                Some(self.code_fill)
            } else {
                has(SPOILER).then_some(self.spoiler_fill)
            },
            // Links keep the text's colour, underlined (`.input a`).
            underline: if has(LINK) {
                Some(UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: false,
                })
            } else {
                has(SPOILER).then_some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(self.subtle),
                    wavy: false,
                })
            },
            ..Default::default()
        }
    }
}

/// A run of `*`, `_` or `~` that may open or close a span.
#[derive(Clone, Copy)]
struct Delim {
    /// Where the part of the run not yet matched starts, and its length.
    pos: usize,
    len: usize,
    /// The run's whole length.
    orig: usize,
    ch: u8,
    open: bool,
    close: bool,
    /// The previous delimiter still in play, or [`NONE`].
    prev: usize,
}

/// A styled span's markers: `**` and `**`, or `[` and `](url)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub open: Range<usize>,
    pub close: Range<usize>,
    pub kind: Kinds,
}

/// The parser, with buffers reused from one edit to the next.
#[derive(Default)]
struct Scanner {
    spans: Vec<(Range<usize>, Kinds)>,
    /// Each matched span's opening and closing markers, for formatting.
    pairs: Vec<Pair>,
    /// Bare URLs (GFM autolink literals).
    bare: Vec<Range<usize>>,
    delims: Vec<Delim>,
    pipes: Vec<usize>,
    /// Each `[` of the paragraph with the `]` that closes it, by `[`, and
    /// each `(` with its `)` on the same line.
    brackets: Vec<(usize, usize)>,
    parens: Vec<(usize, usize)>,
    opens: Vec<usize>,
    events: Vec<(usize, Kinds, bool)>,
}

impl Scanner {
    /// Calls `emit` with each styled run of `text`, in order and
    /// disjoint. Runs over `chips` are left out.
    fn scan(
        &mut self,
        text: &str,
        chips: impl IntoIterator<Item = Range<usize>>,
        mut emit: impl FnMut(Range<usize>, Kinds),
    ) {
        self.spans.clear();
        self.pairs.clear();
        self.bare.clear();
        self.blocks(text);
        for chip in chips {
            self.push(chip, CHIP);
        }
        self.events.clear();
        for (range, kind) in &self.spans {
            self.events.push((range.start, *kind, true));
            self.events.push((range.end, *kind, false));
        }
        self.events.sort_unstable_by_key(|(at, _, _)| *at);
        let mut counts = [0u32; KINDS];
        let mut kinds: Kinds = 0;
        let mut pos = 0;
        let mut run: Option<(Range<usize>, Kinds)> = None;
        for &(at, kind, start) in &self.events {
            if at > pos {
                // Markers stay plain inside the spans they delimit, apart
                // from a code span's fill.
                let shown = if kinds & (MARKER | BLOCK | RULE) != 0 {
                    kinds & (MARKER | BLOCK | RULE | CODE | CHIP)
                } else {
                    kinds
                };
                if shown != 0 && shown & CHIP == 0 {
                    match &mut run {
                        Some((range, k)) if range.end == pos && *k == shown => range.end = at,
                        _ => {
                            if let Some((range, k)) = run.take() {
                                emit(range, k);
                            }
                            run = Some((pos..at, shown));
                        }
                    }
                }
                pos = at;
            }
            let bit = kind.trailing_zeros() as usize;
            if start {
                counts[bit] += 1;
                kinds |= kind;
            } else {
                counts[bit] -= 1;
                if counts[bit] == 0 {
                    kinds &= !kind;
                }
            }
        }
        if let Some((range, k)) = run {
            emit(range, k);
        }
    }

    /// What the field hides after a scan: every marker, plus `pending`;
    /// a bullet shows as a dot and a quote's `>` as a bar.
    fn concealments(&self, text: &str, pending: &[Range<usize>]) -> Vec<Concealment> {
        let mut ranges: Vec<(Range<usize>, Option<&'static str>)> = self
            .spans
            .iter()
            .filter_map(|(range, kind)| match *kind {
                MARKER => Some((range.clone(), None)),
                BLOCK => match &text[range.clone()] {
                    "-" | "*" | "+" => Some((range.clone(), Some("•"))),
                    ">" => Some((range.clone(), Some("▎"))),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        ranges.extend(
            pending
                .iter()
                .filter(|range| range.end <= text.len())
                .map(|range| (range.clone(), None)),
        );
        ranges.sort_by_key(|(range, _)| (range.start, range.end));
        let mut out: Vec<Concealment> = Vec::new();
        for (range, replacement) in ranges {
            match out.last_mut() {
                // Overlapping hidden runs merge; touching ones stay apart so
                // the caret can rest between them.
                Some(last)
                    if last.replacement.is_none()
                        && replacement.is_none()
                        && range.start < last.range.end =>
                {
                    last.range.end = last.range.end.max(range.end);
                }
                Some(last) if range.start < last.range.end => {}
                _ => out.push(match replacement {
                    Some(replacement) => Concealment::new(range).with_replacement(replacement),
                    None => Concealment::new(range),
                }),
            }
        }
        out
    }

    fn pair(&mut self, open: Range<usize>, close: Range<usize>, kind: Kinds) {
        self.pairs.push(Pair { open, close, kind });
    }

    fn push(&mut self, range: Range<usize>, kind: Kinds) {
        if !range.is_empty() {
            self.spans.push((range, kind));
        }
    }

    /// Quotes, lists, rules and fenced code, line by line. The text of each
    /// paragraph goes to [`Self::inline`] as one range, continuation markers
    /// and all: they are only text to it.
    fn blocks(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut paragraph: Option<Range<usize>> = None;
        // The open fence: its character, length and the quotes it is in.
        let mut fence: Option<(u8, usize, usize)> = None;
        // Where the open fence's marker is in `spans`.
        let mut opener = None;
        let mut last_quotes = 0;
        let mut start = 0;
        loop {
            let end = bytes[start..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |i| start + i);
            let mut at = start;
            if let Some((ch, len, quotes)) = fence {
                for _ in 0..quotes {
                    let gt = spaces(bytes, at, end);
                    if bytes.get(gt) != Some(&b'>') || gt >= end {
                        break;
                    }
                    self.push(gt..gt + 1, BLOCK);
                    at = (gt + 1 + usize::from(bytes.get(gt + 1) == Some(&b' '))).min(end);
                }
                let first = spaces(bytes, at, end);
                let run = run_len(bytes, first, end, ch);
                if run >= len && spaces(bytes, first + run, end) == end {
                    self.push(first..end, MARKER);
                    fence = None;
                    opener = None;
                } else {
                    self.push(at..end, CODE);
                }
            } else {
                let mut quotes = 0;
                let mut item = false;
                let mut rule = false;
                loop {
                    let first = spaces(bytes, at, end);
                    if first < end && bytes[first] == b'>' {
                        self.push(first..first + 1, BLOCK);
                        at = first + 1 + usize::from(bytes.get(first + 1) == Some(&b' '));
                        at = at.min(end);
                        quotes += 1;
                    } else if is_rule(bytes, first, end) {
                        self.push(first..end, RULE);
                        at = end;
                        rule = true;
                        break;
                    } else if let Some(marker) = list_marker(bytes, first, end) {
                        self.push(first..marker, BLOCK);
                        at = (marker + 1).min(end);
                        item = true;
                        let task = &bytes[at..end];
                        if (task.starts_with(b"[ ]")
                            || task.starts_with(b"[x]")
                            || task.starts_with(b"[X]"))
                            && matches!(task.get(3), None | Some(b' ' | b'\t'))
                        {
                            self.push(at..at + 3, BLOCK);
                            at = (at + 4).min(end);
                        }
                    } else {
                        break;
                    }
                }
                let first = spaces(bytes, at, end);
                let ticks = run_len(bytes, first, end, b'`');
                let tildes = run_len(bytes, first, end, b'~');
                if rule || first == end {
                    self.flush(text, &mut paragraph);
                } else if ticks >= 3 && !bytes[first + ticks..end].contains(&b'`') {
                    self.flush(text, &mut paragraph);
                    opener = Some(self.fence_opener(first, ticks, end));
                    fence = Some((b'`', ticks, quotes));
                } else if tildes >= 3 {
                    self.flush(text, &mut paragraph);
                    opener = Some(self.fence_opener(first, tildes, end));
                    fence = Some((b'~', tildes, quotes));
                } else {
                    if quotes > 0 {
                        // With its newline, so a quote's lines are one run.
                        self.push(start..(end + 1).min(bytes.len()), QUOTE);
                    }
                    match &mut paragraph {
                        Some(range) if !item && quotes == last_quotes => range.end = end,
                        _ => {
                            self.flush(text, &mut paragraph);
                            paragraph = Some(first..end);
                        }
                    }
                }
                last_quotes = quotes;
            }
            if end == bytes.len() {
                break;
            }
            start = end + 1;
        }
        // A fence still open shows its marker, so the block being typed is
        // plain to see.
        if let Some(at) = opener {
            self.spans[at].1 = CODE;
        }
        self.flush(text, &mut paragraph);
    }

    /// An opening fence of `len` characters at `first`: the fence hides
    /// once closed, the info string after it shows as code. Returns where
    /// the fence's span is.
    fn fence_opener(&mut self, first: usize, len: usize, end: usize) -> usize {
        let at = self.spans.len();
        self.push(first..first + len, MARKER);
        self.push(first + len..end, CODE);
        at
    }

    fn flush(&mut self, text: &str, paragraph: &mut Option<Range<usize>>) {
        if let Some(range) = paragraph.take() {
            self.inline(text, range, false);
        }
    }

    /// Code spans, links, autolinks, spoilers and emphasis in `range`.
    /// Inside a link's label `in_link` is set, since links don't nest.
    fn inline(&mut self, text: &str, range: Range<usize>, in_link: bool) {
        let bytes = text.as_bytes();
        let base = self.delims.len();
        let pipes = self.pipes.len();
        let end = range.end;
        let mut code = CodeSpans::default();
        if !in_link {
            self.match_brackets(bytes, range.clone(), &mut code);
        }
        let mut i = range.start;
        while i < end {
            match bytes[i] {
                b'\\' if i + 1 < end && bytes[i + 1].is_ascii_punctuation() => {
                    self.push(i..i + 1, MARKER);
                    i += 2;
                }
                b'`' => {
                    let n = run_len(bytes, i, end, b'`');
                    if let Some(close) = code.close(bytes, i + n, end, n) {
                        self.push(i..close + n, CODE);
                        self.push(i..i + n, MARKER);
                        self.push(close..close + n, MARKER);
                        self.pair(i..i + n, close..close + n, CODE);
                        i = close + n;
                    } else {
                        i += n;
                    }
                }
                ch @ (b'*' | b'_' | b'~') => {
                    let n = run_len(bytes, i, end, ch);
                    if ch != b'~' || n <= 2 {
                        let before =
                            class((i > range.start).then(|| text[..i].chars().next_back()));
                        let after = class((i + n < end).then(|| text[i + n..].chars().next()));
                        let left = after != Class::Space
                            && (after != Class::Punct || before != Class::Other);
                        let right = before != Class::Space
                            && (before != Class::Punct || after != Class::Other);
                        let (open, close) = if ch == b'_' {
                            (
                                left && (!right || before == Class::Punct),
                                right && (!left || after == Class::Punct),
                            )
                        } else {
                            (left, right)
                        };
                        let prev = if self.delims.len() > base {
                            self.delims.len() - 1
                        } else {
                            NONE
                        };
                        self.delims.push(Delim {
                            pos: i,
                            len: n,
                            orig: n,
                            ch,
                            open,
                            close,
                            prev,
                        });
                    }
                    i += n;
                }
                b'|' if bytes.get(i + 1) == Some(&b'|') && i + 1 < end => {
                    self.pipes.push(i);
                    i += 2;
                }
                b'!' if !in_link && bytes.get(i + 1) == Some(&b'[') => {
                    i = self.link(text, i, i + 1, end).unwrap_or(i + 1);
                }
                b'[' if !in_link => i = self.link(text, i, i, end).unwrap_or(i + 1),
                b'<' => i = self.autolink(bytes, i, end).unwrap_or(i + 1),
                b'h' | b'H' | b'w' | b'W' => {
                    let starts_word = i == range.start
                        || matches!(
                            text[..i].chars().next_back(),
                            Some(c) if c.is_whitespace() || "*_~(".contains(c)
                        );
                    match starts_word.then(|| bare_url(text, i, end)).flatten() {
                        Some(url_end) => {
                            self.push(i..url_end, LINK);
                            self.bare.push(i..url_end);
                            i = url_end;
                        }
                        None => i += 1,
                    }
                }
                _ => i += 1,
            }
        }
        self.emphasis(base);
        self.delims.truncate(base);
        // `||` pairs in order, as `remarkSpoilers` reads them; an empty
        // pair is literal.
        let mut k = pipes;
        while k + 1 < self.pipes.len() {
            let (open, close) = (self.pipes[k], self.pipes[k + 1]);
            if close > open + 2 {
                self.push(open + 2..close, SPOILER);
                self.push(open..open + 2, MARKER);
                self.push(close..close + 2, MARKER);
                self.pair(open..open + 2, close..close + 2, SPOILER);
            }
            k += 2;
        }
        self.pipes.truncate(pipes);
    }

    /// `[label](destination)`, or an image from `!`: returns where it ends.
    fn link(&mut self, text: &str, start: usize, open: usize, end: usize) -> Option<usize> {
        let bytes = text.as_bytes();
        let found = self.brackets.binary_search_by_key(&open, |&(open, _)| open);
        let close = self.brackets[found.ok()?].1;
        if close + 1 >= end || bytes[close + 1] != b'(' {
            return None;
        }
        let found = self
            .parens
            .binary_search_by_key(&(close + 1), |&(open, _)| open);
        let paren = self.parens[found.ok()?].1;
        self.push(start..open + 1, MARKER);
        self.inline(text, open + 1..close, true);
        if start == open {
            self.push(open + 1..close, LINK);
            self.pair(start..open + 1, close..paren + 1, LINK);
        }
        self.push(close..paren + 1, MARKER);
        Some(paren + 1)
    }

    /// Pairs the paragraph's brackets, past escapes and code spans, and its
    /// parentheses, so that finding where a link ends costs no scan.
    fn match_brackets(&mut self, bytes: &[u8], range: Range<usize>, code: &mut CodeSpans) {
        self.brackets.clear();
        self.opens.clear();
        let mut at = range.start;
        while at < range.end {
            match bytes[at] {
                b'\\' => at += 1,
                b'`' => {
                    let n = run_len(bytes, at, range.end, b'`');
                    at = code
                        .close(bytes, at + n, range.end, n)
                        .map_or(at + n, |close| close + n)
                        - 1;
                }
                b'[' => self.opens.push(at),
                b']' => {
                    if let Some(open) = self.opens.pop() {
                        self.brackets.push((open, at));
                    }
                }
                _ => {}
            }
            at += 1;
        }
        self.brackets.sort_unstable();
        self.parens.clear();
        self.opens.clear();
        let mut at = range.start;
        while at < range.end {
            match bytes[at] {
                b'\\' => at += 1,
                b'\n' => self.opens.clear(),
                b'(' => self.opens.push(at),
                b')' => {
                    if let Some(open) = self.opens.pop() {
                        self.parens.push((open, at));
                    }
                }
                _ => {}
            }
            at += 1;
        }
        self.parens.sort_unstable();
    }

    /// `<scheme:…>` or `<name@host>`.
    fn autolink(&mut self, bytes: &[u8], open: usize, end: usize) -> Option<usize> {
        // An autolink holds no space or `<`, so the search stops there.
        let close = open
            + 1
            + bytes[open + 1..end]
                .iter()
                .position(|&b| b == b'>' || b == b'<' || b.is_ascii_whitespace())?;
        if bytes[close] != b'>' {
            return None;
        }
        let inner = &bytes[open + 1..close];
        let scheme = inner.iter().position(|&b| b == b':');
        let uri = scheme.is_some_and(|colon| {
            (2..=32).contains(&colon)
                && inner[0].is_ascii_alphabetic()
                && inner[..colon]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(b))
        });
        let email = inner.contains(&b'@');
        if !(uri || email) {
            return None;
        }
        self.push(open..open + 1, MARKER);
        self.push(open + 1..close, LINK);
        self.push(close..close + 1, MARKER);
        Some(close + 1)
    }

    /// CommonMark's "process emphasis", over the delimiters from `base`.
    fn emphasis(&mut self, base: usize) {
        // The lowest delimiter a closer may still match, per character,
        // closer length mod 3 and whether the closer can open.
        let mut bottom = [base; 18];
        let mut c = base;
        while c < self.delims.len() {
            let closer = self.delims[c];
            if !closer.close || closer.len == 0 {
                c += 1;
                continue;
            }
            let key = match closer.ch {
                b'*' => 0,
                b'_' => 6,
                _ => 12,
            } + closer.orig % 3 * 2
                + usize::from(closer.open);
            let mut o = closer.prev;
            while o != NONE && o >= bottom[key] {
                let opener = &self.delims[o];
                if opener.ch == closer.ch && opener.open && opener.len > 0 && fits(opener, &closer)
                {
                    break;
                }
                o = opener.prev;
            }
            if o == NONE || o < bottom[key] {
                bottom[key] = c;
                if !closer.open {
                    self.unlink(c);
                }
                c += 1;
                continue;
            }
            let opener = self.delims[o];
            let (n, kind) = match closer.ch {
                b'~' => (closer.len, STRIKE),
                _ if opener.len >= 2 && closer.len >= 2 => (2, STRONG),
                _ => (1, EMPHASIS),
            };
            let inner = opener.pos + opener.len..closer.pos;
            self.push(inner.start - n..inner.start, MARKER);
            self.push(inner.end..inner.end + n, MARKER);
            self.pair(inner.start - n..inner.start, inner.end..inner.end + n, kind);
            self.push(inner, kind);
            self.delims[o].len -= n;
            self.delims[c].pos += n;
            self.delims[c].len -= n;
            // The delimiters between them are literal now.
            self.delims[c].prev = if self.delims[o].len == 0 {
                opener.prev
            } else {
                o
            };
            if self.delims[c].len == 0 {
                self.unlink(c);
                c += 1;
            }
        }
    }

    /// Takes delimiter `c` out of play.
    fn unlink(&mut self, c: usize) {
        if let Some(next) = self.delims.get(c + 1).map(|d| d.prev)
            && next == c
        {
            self.delims[c + 1].prev = self.delims[c].prev;
        }
    }
}

/// Whether `opener` may close with `closer`: strikethrough needs equal
/// runs, and emphasis follows the rule of three.
fn fits(opener: &Delim, closer: &Delim) -> bool {
    if closer.ch == b'~' {
        return opener.orig == closer.orig;
    }
    !((opener.close || closer.open)
        && (opener.orig + closer.orig).is_multiple_of(3)
        && !(opener.orig.is_multiple_of(3) && closer.orig.is_multiple_of(3)))
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Space,
    Punct,
    Other,
}

/// A neighbour of a delimiter run; the line's edges count as space.
fn class(c: Option<Option<char>>) -> Class {
    match c.flatten() {
        None => Class::Space,
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric()) => {
            Class::Punct
        }
        Some(_) => Class::Other,
    }
}

fn spaces(bytes: &[u8], mut at: usize, end: usize) -> usize {
    while at < end && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    at
}

fn run_len(bytes: &[u8], at: usize, end: usize, ch: u8) -> usize {
    bytes[at.min(end)..end]
        .iter()
        .take_while(|&&b| b == ch)
        .count()
}

/// `***`, `---` or `___`, spaces allowed between.
fn is_rule(bytes: &[u8], first: usize, end: usize) -> bool {
    let Some(&ch @ (b'*' | b'-' | b'_')) = bytes.get(first).filter(|_| first < end) else {
        return false;
    };
    let mut count = 0;
    for &b in &bytes[first..end] {
        match b {
            b' ' | b'\t' => {}
            _ if b == ch => count += 1,
            _ => return false,
        }
    }
    count >= 3
}

/// The end of a `-`, `*`, `+`, `1.` or `1)` marker followed by a space or
/// the end of the line.
fn list_marker(bytes: &[u8], first: usize, end: usize) -> Option<usize> {
    let marker = match bytes.get(first).filter(|_| first < end)? {
        b'-' | b'*' | b'+' => first + 1,
        b'0'..=b'9' => {
            let digits = bytes[first..end]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            let at = first + digits;
            if digits > 9 || at >= end || !matches!(bytes[at], b'.' | b')') {
                return None;
            }
            at + 1
        }
        _ => return None,
    };
    (marker == end || matches!(bytes[marker], b' ' | b'\t')).then_some(marker)
}

/// Code span ends within one range. A run of `n` backticks with no
/// closer after one point has none after any later point either, so a
/// failed search is remembered and many unclosed runs cost one scan.
struct CodeSpans {
    failed: [usize; 8],
}

impl Default for CodeSpans {
    fn default() -> Self {
        Self {
            failed: [usize::MAX; 8],
        }
    }
}

impl CodeSpans {
    /// Where a run of exactly `n` backticks after `at` closes a span.
    fn close(&mut self, bytes: &[u8], mut at: usize, end: usize, n: usize) -> Option<usize> {
        let from = at;
        if self.failed.get(n).is_some_and(|&failed| from >= failed) {
            return None;
        }
        while at < end {
            if bytes[at] == b'`' {
                let run = run_len(bytes, at, end, b'`');
                if run == n {
                    return Some(at);
                }
                at += run;
            } else {
                at += 1;
            }
        }
        if let Some(failed) = self.failed.get_mut(n) {
            *failed = (*failed).min(from);
        }
        None
    }
}

/// A GFM autolink literal at `at`: `http://`, `https://` or `www.` and a
/// domain with a dot. Returns where it ends.
fn bare_url(text: &str, at: usize, end: usize) -> Option<usize> {
    let rest = &text.as_bytes()[at..end];
    let prefix = if rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"https://") {
        8
    } else if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"http://") {
        7
    } else if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"www.") {
        0
    } else {
        return None;
    };
    let len = rest
        .iter()
        .position(|&b| b.is_ascii_whitespace() || b == b'<')
        .unwrap_or(rest.len());
    let domain = rest[prefix..len]
        .iter()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b"-_.".contains(&b) || b >= 0x80)
        .filter(|&&b| b == b'.')
        .count();
    if domain == 0 {
        return None;
    }
    // Trailing punctuation ends the sentence, not the link, and so does a
    // `)` the link didn't open.
    let mut len = len;
    loop {
        match rest[..len].last() {
            Some(b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' | b'\'' | b'"') => len -= 1,
            Some(b')') => {
                let open = rest[..len].iter().filter(|&&b| b == b'(').count();
                let close = rest[..len].iter().filter(|&&b| b == b')').count();
                if close > open {
                    len -= 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    // `www.` needs a domain after it.
    (len > prefix.max(4)).then_some(at + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The styled runs of `text`, as `(text, kinds)`.
    fn runs(text: &str) -> Vec<(&str, Kinds)> {
        chips(text, None)
    }

    /// Typing a draft one character at a time, with one scanner, styles
    /// each prefix as a fresh scanner would.
    #[test]
    fn reuses_its_buffers_while_typing() {
        let text = "Plain, **bold**, *italic*, _also italic_, ~~struck~~ and ***both***.\n\
            Code `let x = 1;` and a [link label](https://example.com/docs) or https://buzz.dev/x.\n\
            > A quoted line with **bold** inside\n- a bullet item\n1. a numbered item\n\
            - [x] a done task and ||a spoiler||\n```\nfenced *code*\n```\n\
            snake_case_name and 2 * 3 * 4 stay plain, \\*escaped\\* too";
        let mut scanner = Scanner::default();
        for (at, _) in text.char_indices() {
            let prefix = &text[..at];
            let mut reused = Vec::new();
            scanner.scan(prefix, None, |range, kinds| reused.push((range, kinds)));
            assert!(
                scanner
                    .pairs
                    .iter()
                    .all(|pair| pair.open.end <= pair.close.start)
            );
            let fresh: Vec<_> = runs(prefix)
                .into_iter()
                .map(|(run, kinds)| {
                    let start = run.as_ptr() as usize - prefix.as_ptr() as usize;
                    (start..start + run.len(), kinds)
                })
                .collect();
            assert_eq!(reused, fresh, "{prefix:?}");
        }
    }

    fn chips(text: &str, chips: Option<Range<usize>>) -> Vec<(&str, Kinds)> {
        let mut out = Vec::new();
        Scanner::default().scan(text, chips, |range, kinds| out.push((&text[range], kinds)));
        out
    }

    #[test]
    fn styles_emphasis_and_subdues_its_markers() {
        assert_eq!(
            runs("a **bold** and *it* or _it_ ~~gone~~ ~one~"),
            vec![
                ("**", MARKER),
                ("bold", STRONG),
                ("**", MARKER),
                ("*", MARKER),
                ("it", EMPHASIS),
                ("*", MARKER),
                ("_", MARKER),
                ("it", EMPHASIS),
                ("_", MARKER),
                ("~~", MARKER),
                ("gone", STRIKE),
                ("~~", MARKER),
                ("~", MARKER),
                ("one", STRIKE),
                ("~", MARKER),
            ]
        );
    }

    #[test]
    fn nests_emphasis_like_commonmark() {
        assert_eq!(
            runs("***both***"),
            vec![
                ("***", MARKER),
                ("both", STRONG | EMPHASIS),
                ("***", MARKER),
            ]
        );
        assert_eq!(
            runs("**a *b* c**"),
            vec![
                ("**", MARKER),
                ("a ", STRONG),
                ("*", MARKER),
                ("b", STRONG | EMPHASIS),
                ("*", MARKER),
                (" c", STRONG),
                ("**", MARKER),
            ]
        );
        // The rule of three keeps `*foo**` from closing as emphasis.
        assert_eq!(
            runs("*foo**bar**baz*"),
            vec![
                ("*", MARKER),
                ("foo", EMPHASIS),
                ("**", MARKER),
                ("bar", EMPHASIS | STRONG),
                ("**", MARKER),
                ("baz", EMPHASIS),
                ("*", MARKER),
            ]
        );
    }

    #[test]
    fn leaves_literal_delimiters_alone() {
        for text in [
            "snake_case_name",
            "2 * 3 * 4",
            "a ** b",
            "**unclosed",
            "~~a~",
            "* not emphasis*",
            "\\*escaped*",
        ] {
            assert!(
                runs(text)
                    .iter()
                    .all(|(_, kinds)| kinds & !MARKER == 0 || *kinds == BLOCK),
                "{text}: {:?}",
                runs(text)
            );
        }
        assert_eq!(runs("\\*a*"), vec![("\\", MARKER)]);
        assert_eq!(runs("intra*word*emphasis")[1], ("word", EMPHASIS));
    }

    #[test]
    fn code_spans_win_over_emphasis() {
        assert_eq!(
            runs("*a `*` b* ``x ` y``"),
            vec![
                ("*", MARKER),
                ("a ", EMPHASIS),
                ("`", CODE | MARKER),
                ("*", EMPHASIS | CODE),
                ("`", CODE | MARKER),
                (" b", EMPHASIS),
                ("*", MARKER),
                ("``", CODE | MARKER),
                ("x ` y", CODE),
                ("``", CODE | MARKER),
            ]
        );
        assert_eq!(runs("`unclosed"), vec![]);
    }

    #[test]
    fn links_show_their_label_and_subdue_the_rest() {
        assert_eq!(
            runs("see [the **docs**](https://a.b/(c)) now"),
            vec![
                ("[", MARKER),
                ("the ", LINK),
                ("**", MARKER),
                ("docs", LINK | STRONG),
                ("**](https://a.b/(c))", MARKER),
            ]
        );
        assert_eq!(
            runs("![alt](x.png) <https://a.b> [no link]"),
            vec![
                ("![", MARKER),
                ("](x.png)", MARKER),
                ("<", MARKER),
                ("https://a.b", LINK),
                (">", MARKER),
            ]
        );
        // Emphasis can span a link but not reach into its label.
        assert_eq!(
            runs("*a [b*](c)")
                .iter()
                .filter(|(_, kinds)| kinds & EMPHASIS != 0)
                .count(),
            0
        );
    }

    #[test]
    fn finds_bare_urls() {
        assert_eq!(
            runs("go to https://example.com/a_(b). or www.x.org, (http://y.io)"),
            vec![
                ("https://example.com/a_(b)", LINK),
                ("www.x.org", LINK),
                ("http://y.io", LINK),
            ]
        );
        assert_eq!(runs("https://localhost, xhttp://a.b and www."), vec![]);
    }

    #[test]
    fn marks_quotes_lists_and_tasks() {
        assert_eq!(
            runs("> quoted *a\n> b*\n- item\n  1. nested\n- [x] done\n---"),
            vec![
                (">", BLOCK),
                (" quoted ", QUOTE),
                ("*", MARKER),
                ("a\n", QUOTE | EMPHASIS),
                (">", BLOCK),
                (" b", QUOTE | EMPHASIS),
                ("*", MARKER),
                ("\n", QUOTE),
                ("-", BLOCK),
                ("1.", BLOCK),
                ("-", BLOCK),
                ("[x]", BLOCK),
                ("---", RULE),
            ]
        );
        // A new item or a blank line ends the paragraph.
        assert_eq!(runs("- *a\n- b*"), vec![("-", BLOCK), ("-", BLOCK)]);
        assert_eq!(runs("*a\n\nb*"), vec![]);
        assert_eq!(runs("-1 and 2. no"), vec![]);
    }

    #[test]
    fn fenced_code_is_code_until_it_closes() {
        assert_eq!(
            runs("```rust\nlet *a* = 1;\n```\n*b*"),
            vec![
                ("```", MARKER),
                ("rust", CODE),
                ("let *a* = 1;", CODE),
                ("```", MARKER),
                ("*", MARKER),
                ("b", EMPHASIS),
                ("*", MARKER),
            ]
        );
        assert_eq!(
            runs("> ~~~\n> code\n> ~~~\nafter *x*"),
            vec![
                (">", BLOCK),
                ("~~~", MARKER),
                (">", BLOCK),
                ("code", CODE),
                (">", BLOCK),
                ("~~~", MARKER),
                ("*", MARKER),
                ("x", EMPHASIS),
                ("*", MARKER),
            ]
        );
        // An unclosed fence runs to the end, as it renders, and shows its
        // fence and what follows it while it is typed.
        assert_eq!(
            runs("```\na\n\nb"),
            vec![("```", CODE), ("a", CODE), ("b", CODE)]
        );
        assert_eq!(runs("```hello"), vec![("```hello", CODE)]);
        // Not strikethrough: an unclosed fence, shown as the code it sends as.
        assert_eq!(runs("~~~three~~~"), vec![("~~~three~~~", CODE)]);
        // Three backticks with more on the line are a code span.
        assert_eq!(runs("```a```")[1], ("a", CODE));
    }

    fn active(text: &str, selection: Range<usize>) -> Active {
        let mut runs = Vec::new();
        Scanner::default().scan(text, None, |range, kinds| runs.push((range, kinds)));
        active_in(&runs, selection, text, 0)
    }

    #[test]
    fn toolbar_state_follows_the_caret_and_selection() {
        // A caret takes the formats of the character before it.
        assert!(active("a **bold** b", 7..7).bold);
        assert!(!active("a **bold** b", 2..2).bold);
        // A selection is pressed only when all of its text has the format;
        // hidden markers inside it don't count as text.
        let text = "_start **strong** plain_";
        let all = active(text, 0..text.len());
        assert!(all.italic && !all.bold);
        assert!(!active("x _y_", 0..5).italic);
        assert!(active("[a link](https://x.y)", 1..7).link);
    }

    #[test]
    fn styles_intraword_strong_in_quotes() {
        assert_eq!(
            runs("> a quote**now**"),
            vec![
                (">", BLOCK),
                (" a quote", QUOTE),
                ("**", MARKER),
                ("now", QUOTE | STRONG),
                ("**", MARKER),
            ]
        );
    }

    #[test]
    fn styles_spoilers() {
        assert_eq!(
            runs("a ||secret|| b |||| c"),
            vec![("||", MARKER), ("secret", SPOILER), ("||", MARKER)]
        );
    }

    #[test]
    fn leaves_chips_unstyled() {
        let text = "**@alice_b and x**";
        assert_eq!(
            chips(text, Some(2..10)),
            vec![("**", MARKER), (" and x", STRONG), ("**", MARKER)]
        );
    }

    #[test]
    fn handles_multibyte_text() {
        assert_eq!(
            runs("é*ü*ö 👍**ok**"),
            vec![
                ("*", MARKER),
                ("ü", EMPHASIS),
                ("*", MARKER),
                ("**", MARKER),
                ("ok", STRONG),
                ("**", MARKER),
            ]
        );
    }
}
