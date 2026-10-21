//! The formatting toolbar's edits (`ComposerFormattingTools.tsx`,
//! `EditableInput.tsx` `toggleFormat`). The composer edits Markdown source
//! and hides the markers, so a format is a pair of delimiters around text:
//! toggling adds or removes them as ProseMirror's `toggleMark` adds or
//! removes a mark. Edits are insertions and deletions in the original
//! text's offsets, so chips elsewhere survive.

use super::live_markdown::{CODE, EMPHASIS, Kinds, Pair, SPOILER, STRIKE, STRONG};
use buzz_ui::IconName;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Bold,
    Italic,
    Strike,
    Code,
    CodeBlock,
    Link,
    Bullet,
    Ordered,
    Quote,
    Spoiler,
}

impl Format {
    /// The toolbar's order (§4.8).
    pub const ALL: [Format; 10] = [
        Format::Bold,
        Format::Italic,
        Format::Strike,
        Format::Code,
        Format::CodeBlock,
        Format::Link,
        Format::Bullet,
        Format::Ordered,
        Format::Quote,
        Format::Spoiler,
    ];

    pub fn icon(self) -> IconName {
        match self {
            Format::Bold => IconName::TextB,
            Format::Italic => IconName::TextItalic,
            Format::Strike => IconName::TextStrikethrough,
            Format::Code => IconName::Code,
            Format::CodeBlock => IconName::CodeBlock,
            Format::Link => IconName::Link,
            Format::Bullet => IconName::ListBullets,
            Format::Ordered => IconName::ListNumbers,
            Format::Quote => IconName::Quotes,
            Format::Spoiler => IconName::Detective,
        }
    }
}

#[derive(Debug)]
pub struct Edit {
    /// Ascending, non-overlapping; apply from the last.
    pub changes: Vec<(Range<usize>, String)>,
    pub selection: Range<usize>,
}

fn marker(format: Format, text: &str, selection: &Range<usize>) -> Option<&'static str> {
    Some(match format {
        Format::Bold => "**",
        Format::Italic => {
            // `_` inside a word is literal; `*` binds there (§5d.3).
            let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
            if word(text[..selection.start].chars().next_back())
                || word(text[selection.end..].chars().next())
            {
                "*"
            } else {
                "_"
            }
        }
        Format::Strike => "~~",
        Format::Code => "`",
        Format::Spoiler => "||",
        _ => return None,
    })
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

/// The selected lines' start offsets and the length of their list or
/// quote prefix, if they have this format's.
fn lines(text: &str, selection: &Range<usize>, format: Format) -> Vec<(usize, Option<usize>)> {
    let start = line_start(text, selection.start);
    let end = line_end(text, selection.end.max(selection.start));
    let mut out = Vec::new();
    let mut at = start;
    for line in text[start..end].split('\n') {
        let prefix = match format {
            Format::Bullet => ["- ", "* "]
                .iter()
                .find(|p| line.starts_with(**p))
                .map(|p| p.len()),
            Format::Quote => line
                .starts_with("> ")
                .then_some(2)
                .or(line.starts_with('>').then_some(1)),
            Format::Ordered => {
                let digits = line.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0 && line[digits..].starts_with(". ")).then_some(digits + 2)
            }
            _ => None,
        };
        out.push((at, prefix));
        at += line.len() + 1;
    }
    out
}

pub fn active(text: &str, selection: Range<usize>, format: Format) -> bool {
    if selection.end > text.len() {
        return false;
    }
    match format {
        Format::Bullet | Format::Ordered | Format::Quote => lines(text, &selection, format)
            .iter()
            .all(|(_, prefix)| prefix.is_some()),
        Format::CodeBlock => {
            text[..selection.start].ends_with("```\n") && text[selection.end..].starts_with("\n```")
        }
        Format::Link => false,
        _ => {
            let Some(m) = marker(format, text, &selection) else {
                return false;
            };
            text[..selection.start].ends_with(m) && text[selection.end..].starts_with(m)
        }
    }
}

pub fn apply(text: &str, selection: Range<usize>, format: Format) -> Edit {
    let selection = selection.start.min(text.len())..selection.end.min(text.len());
    match format {
        Format::Bullet | Format::Ordered | Format::Quote => {
            let lines = lines(text, &selection, format);
            let remove = lines.iter().all(|(_, prefix)| prefix.is_some());
            let mut changes = Vec::new();
            let mut shift_start = 0isize;
            let mut shift_end = 0isize;
            for (index, (at, prefix)) in lines.iter().enumerate() {
                let delta = if remove {
                    let length = prefix.expect("every line has the prefix");
                    changes.push((*at..*at + length, String::new()));
                    -(length as isize)
                } else {
                    let insert = match format {
                        Format::Bullet => "- ".to_string(),
                        Format::Quote => "> ".to_string(),
                        _ => format!("{}. ", index + 1),
                    };
                    let length = insert.len() as isize;
                    changes.push((*at..*at, insert));
                    length
                };
                if *at <= selection.start {
                    shift_start += delta;
                }
                if *at <= selection.end {
                    shift_end += delta;
                }
            }
            let start = (selection.start as isize + shift_start).max(0) as usize;
            let end = (selection.end as isize + shift_end).max(start as isize) as usize;
            Edit {
                changes,
                selection: start..end,
            }
        }
        Format::CodeBlock => {
            if active(text, selection.clone(), format) {
                return Edit {
                    changes: vec![
                        (selection.start - 4..selection.start, String::new()),
                        (selection.end..selection.end + 4, String::new()),
                    ],
                    selection: selection.start - 4..selection.end - 4,
                };
            }
            let open = if selection.start == 0 || text[..selection.start].ends_with('\n') {
                "```\n"
            } else {
                "\n```\n"
            };
            let close = if selection.end == text.len() || text[selection.end..].starts_with('\n') {
                "\n```"
            } else {
                "\n```\n"
            };
            Edit {
                selection: selection.start + open.len()..selection.end + open.len(),
                changes: vec![
                    (selection.start..selection.start, open.into()),
                    (selection.end..selection.end, close.into()),
                ],
            }
        }
        Format::Link => {
            const URL: &str = "https://";
            let label = selection.end - selection.start;
            let url_at = selection.start + 1 + label + 2;
            Edit {
                changes: vec![
                    (selection.start..selection.start, "[".into()),
                    (selection.end..selection.end, format!("]({URL})")),
                ],
                selection: if label == 0 {
                    selection.start + 1..selection.start + 1
                } else {
                    url_at..url_at + URL.len()
                },
            }
        }
        _ => {
            let m = marker(format, text, &selection).expect("an inline format");
            let n = m.len();
            if active(text, selection.clone(), format) {
                return Edit {
                    changes: vec![
                        (selection.start - n..selection.start, String::new()),
                        (selection.end..selection.end + n, String::new()),
                    ],
                    selection: selection.start - n..selection.end - n,
                };
            }
            let inner = &text[selection.clone()];
            if inner.len() >= 2 * n && inner.starts_with(m) && inner.ends_with(m) {
                return Edit {
                    changes: vec![
                        (selection.start..selection.start + n, String::new()),
                        (selection.end - n..selection.end, String::new()),
                    ],
                    selection: selection.start..selection.end - 2 * n,
                };
            }
            Edit {
                changes: vec![
                    (selection.start..selection.start, m.into()),
                    (selection.end..selection.end, m.into()),
                ],
                selection: selection.start + n..selection.end + n,
            }
        }
    }
}

#[cfg(test)]
pub fn run(text: &str, selection: Range<usize>, format: Format) -> (String, Range<usize>) {
    let edit = apply(text, selection, format);
    let mut out = text.to_string();
    for (range, replacement) in edit.changes.iter().rev() {
        out.replace_range(range.clone(), replacement);
    }
    (out, edit.selection)
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Bold => "Bold",
            Format::Italic => "Italic",
            Format::Strike => "Strikethrough",
            Format::Code => "Code",
            Format::CodeBlock => "Code block",
            Format::Link => "Link",
            Format::Bullet => "Bullet list",
            Format::Ordered => "Ordered list",
            Format::Quote => "Quote",
            Format::Spoiler => "Spoiler",
        }
    }

    /// The chord: its key, and whether it adds Shift or Alt to the
    /// platform's command key (`composerFormats`; Link is Mod+K).
    pub fn binding(self) -> (&'static str, bool, bool) {
        match self {
            Format::Bold => ("B", false, false),
            Format::Italic => ("I", false, false),
            Format::Strike => ("X", true, false),
            Format::Code => ("E", false, false),
            Format::CodeBlock => ("C", false, true),
            Format::Link => ("K", false, false),
            Format::Bullet => ("8", true, false),
            Format::Ordered => ("7", true, false),
            Format::Quote => ("B", true, false),
            Format::Spoiler => ("P", true, false),
        }
    }

    /// "Bold (Ctrl+B)", or "Bold (⌘B)" on macOS (`formatBinding`).
    pub fn tooltip(self) -> String {
        let (key, shift, alt) = self.binding();
        format!(
            "{} ({})",
            self.label(),
            chord(key, shift, alt, cfg!(target_os = "macos"))
        )
    }

    /// The span kind an inline format styles.
    pub fn kind(self) -> Option<Kinds> {
        Some(match self {
            Format::Bold => STRONG,
            Format::Italic => EMPHASIS,
            Format::Strike => STRIKE,
            Format::Code => CODE,
            Format::Spoiler => SPOILER,
            _ => return None,
        })
    }
}

/// A key chord as `formatBinding` writes it: `⌥⇧⌘K` on Apple platforms,
/// `Ctrl+Alt+Shift+K` elsewhere.
fn chord(key: &str, shift: bool, alt: bool, apple: bool) -> String {
    if apple {
        format!(
            "{}{}⌘{key}",
            if alt { "⌥" } else { "" },
            if shift { "⇧" } else { "" }
        )
    } else {
        let mut parts = vec!["Ctrl"];
        if alt {
            parts.push("Alt");
        }
        if shift {
            parts.push("Shift");
        }
        parts.push(key);
        parts.join("+")
    }
}

/// Where `offset` lands after `changes`; `after` puts it past text
/// inserted exactly there.
fn map(offset: usize, changes: &[(Range<usize>, String)], after: bool) -> usize {
    let mut out = offset as isize;
    for (range, text) in changes {
        let delta = text.len() as isize - range.len() as isize;
        if range.end < offset || (range.end == offset && (after || !range.is_empty())) {
            out += delta;
        } else if range.start < offset {
            // Inside a replaced range: to its start.
            out -= (offset - range.start) as isize;
        }
    }
    out.max(0) as usize
}

fn finish(mut changes: Vec<(Range<usize>, String)>, selection: Range<usize>) -> Edit {
    changes.sort_by_key(|(range, _)| (range.start, range.end));
    let start = map(selection.start, &changes, true);
    let end = map(selection.end, &changes, selection.is_empty());
    Edit {
        selection: start..end.max(start),
        changes,
    }
}

/// The delimiter that adds `format` around `range`: `_` for italics
/// between spaces, `*` where a word continues (`_` is literal there).
fn delimiter(format: Format, text: &str, range: &Range<usize>) -> &'static str {
    match format {
        Format::Bold => "**",
        Format::Italic => {
            let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
            if word(text[..range.start].chars().next_back())
                || word(text[range.end..].chars().next())
            {
                "*"
            } else {
                "_"
            }
        }
        Format::Strike => "~~",
        Format::Code => "`",
        _ => "||",
    }
}

/// `range` without its leading and trailing whitespace, so delimiters
/// hug the text and still open and close.
fn trimmed(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let start = range.start + (slice.len() - slice.trim_start().len());
    let end = range.end - (slice.len() - slice.trim_end().len());
    start..end.max(start)
}

/// Toggles an inline format over a selection, as `toggleMark` with
/// `removeWhenPresent: false`: when all of it has the format (`all`),
/// the format comes off, splitting the spans it was part of; otherwise
/// the selection and the spans it touches are joined into one.
pub fn toggle_range(
    text: &str,
    selection: Range<usize>,
    format: Format,
    pairs: &[Pair],
    all: bool,
) -> Edit {
    let overlapping: Vec<&Pair> = pairs
        .iter()
        .filter(|pair| pair.open.start < selection.end && pair.close.end > selection.start)
        .collect();
    let mut changes: Vec<(Range<usize>, String)> = Vec::new();
    if all {
        for pair in overlapping {
            let marker = &text[pair.open.clone()];
            // `_` cannot close inside a word, so a split uses `*`.
            let replace = if marker.starts_with('_') {
                "*".repeat(marker.len())
            } else {
                marker.to_string()
            };
            let left = trimmed(text, pair.open.end..selection.start.max(pair.open.end));
            let right = trimmed(text, selection.end.min(pair.close.start)..pair.close.start);
            let keep_left = pair.open.end < selection.start && !left.is_empty();
            let keep_right = selection.end < pair.close.start && !right.is_empty();
            if keep_left {
                if replace != marker {
                    changes.push((pair.open.clone(), replace.clone()));
                }
                changes.push((left.end..left.end, replace.clone()));
            } else {
                changes.push((pair.open.clone(), String::new()));
            }
            if keep_right {
                changes.push((right.start..right.start, replace.clone()));
                if replace != marker {
                    changes.push((pair.close.clone(), replace.clone()));
                }
            } else {
                changes.push((pair.close.clone(), String::new()));
            }
        }
        return finish(changes, selection);
    }
    let mut union = selection.clone();
    for pair in &overlapping {
        union.start = union.start.min(pair.open.start);
        union.end = union.end.max(pair.close.end);
        changes.push((pair.open.clone(), String::new()));
        changes.push((pair.close.clone(), String::new()));
    }
    let inner = trimmed(text, union);
    if inner.is_empty() {
        return finish(Vec::new(), selection);
    }
    // Delimiters go outside any deleted markers at the edges.
    let start = changes
        .iter()
        .filter(|(range, _)| range.start <= inner.start && range.end > inner.start)
        .map(|(range, _)| range.end)
        .max()
        .unwrap_or(inner.start);
    let end = changes
        .iter()
        .filter(|(range, _)| range.start < inner.end && range.end >= inner.end)
        .map(|(range, _)| range.start)
        .min()
        .unwrap_or(inner.end);
    let marker = delimiter(format, text, &(start..end));
    changes.push((start..start, marker.to_string()));
    changes.push((end..end, marker.to_string()));
    finish(changes, selection)
}

/// A collapsed-caret toggle: an empty delimiter pair at the caret, which
/// the composer keeps hidden until text fills it. Inside a span of the
/// format the same pair ends the span and starts it again after the
/// caret, so text typed there has no format.
pub fn pending_pair(
    text: &str,
    caret: usize,
    format: Format,
    inside: bool,
) -> (Edit, &'static str) {
    let marker = if inside && format == Format::Italic {
        "*"
    } else {
        delimiter(format, text, &(caret..caret))
    };
    let n = marker.len();
    let edit = Edit {
        changes: vec![(caret..caret, format!("{marker}{marker}"))],
        selection: caret + n..caret + n,
    };
    (edit, marker)
}

/// Where `offset` lands after `changes`.
pub fn map_offset(offset: usize, changes: &[(Range<usize>, String)]) -> usize {
    map(offset, changes, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_unwraps_inline_formats() {
        assert_eq!(
            run("say hi", 4..6, Format::Bold),
            ("say **hi**".into(), 6..8)
        );
        assert_eq!(
            run("say **hi**", 6..8, Format::Bold),
            ("say hi".into(), 4..6)
        );
        assert_eq!(
            run("say **hi**", 4..10, Format::Bold),
            ("say hi".into(), 4..6)
        );
        assert_eq!(run("", 0..0, Format::Code), ("``".into(), 1..1));
        assert_eq!(run("a b", 2..3, Format::Italic), ("a _b_".into(), 3..4));
        assert_eq!(run("ab", 1..2, Format::Italic), ("a*b*".into(), 2..3));
        assert_eq!(run("x", 0..1, Format::Strike), ("~~x~~".into(), 2..3));
        assert_eq!(run("x", 0..1, Format::Spoiler), ("||x||".into(), 2..3));
        assert_eq!(
            run("see docs", 4..8, Format::Link),
            ("see [docs](https://)".into(), 11..19)
        );
    }

    #[test]
    fn prefixes_and_unprefixes_lines() {
        assert_eq!(run("a\nb", 0..3, Format::Bullet), ("- a\n- b".into(), 2..7));
        assert_eq!(run("- a\n- b", 2..7, Format::Bullet), ("a\nb".into(), 0..3));
        assert_eq!(
            run("a\nb", 0..3, Format::Ordered),
            ("1. a\n2. b".into(), 3..9)
        );
        assert_eq!(
            run("x\nquote", 3..3, Format::Quote),
            ("x\n> quote".into(), 5..5)
        );
        assert_eq!(
            run("code", 0..4, Format::CodeBlock),
            ("```\ncode\n```".into(), 4..8)
        );
        assert_eq!(
            run("```\ncode\n```", 4..8, Format::CodeBlock),
            ("code".into(), 0..4)
        );
        assert_eq!(
            run("see code here", 4..8, Format::CodeBlock),
            ("see \n```\ncode\n```\n here".into(), 9..13)
        );
    }

    fn toggle(
        text: &str,
        selection: Range<usize>,
        format: Format,
        all: bool,
    ) -> (String, Range<usize>) {
        use super::super::live_markdown::LiveMarkdown;
        let pairs = LiveMarkdown::scan_pairs(text, format.kind().unwrap());
        let edit = toggle_range(text, selection, format, &pairs, all);
        let mut out = text.to_string();
        for (range, replacement) in edit.changes.iter().rev() {
            out.replace_range(range.clone(), replacement);
        }
        (out, edit.selection)
    }

    #[test]
    fn toggles_marks_like_prosemirror() {
        // Adding wraps the trimmed selection.
        assert_eq!(
            toggle("say hi ", 4..7, Format::Bold, false),
            ("say **hi** ".into(), 6..11)
        );
        // Removing a whole span drops its markers.
        assert_eq!(
            toggle("say **hi**", 6..8, Format::Bold, true),
            ("say hi".into(), 4..6)
        );
        // Removing from the middle splits the span around the selection.
        assert_eq!(
            toggle("**one two three**", 6..9, Format::Bold, true),
            ("**one** two **three**".into(), 8..11)
        );
        // An italic `_` span splits with `*`, which closes inside words.
        assert_eq!(
            toggle("_abcd_", 2..4, Format::Italic, true),
            ("*a*bc*d*".into(), 3..5)
        );
        // Adding across spans joins them into one.
        assert_eq!(
            toggle("**a** b **c**", 3..10, Format::Bold, false).0,
            "**a b c**"
        );
        assert_eq!(
            toggle("ab", 1..2, Format::Italic, false),
            ("a*b*".into(), 2..3)
        );
    }

    #[test]
    fn pending_pairs_wait_at_the_caret() {
        let (edit, marker) = pending_pair("hi ", 3, Format::Bold, false);
        assert_eq!(edit.changes, vec![(3..3, "****".to_string())]);
        assert_eq!((edit.selection, marker), (5..5, "**"));
        let (edit, _) = pending_pair("_ab_", 2, Format::Italic, true);
        assert_eq!(edit.changes, vec![(2..2, "**".to_string())]);
    }

    #[test]
    fn tooltips_show_the_platform_chord() {
        assert_eq!(chord("B", false, false, false), "Ctrl+B");
        assert_eq!(chord("C", false, true, false), "Ctrl+Alt+C");
        assert_eq!(chord("X", true, false, true), "⇧⌘X");
        assert_eq!(chord("C", false, true, true), "⌥⌘C");
        assert_eq!(Format::Link.label(), "Link");
        assert_eq!(Format::Link.binding(), ("K", false, false));
    }
}
