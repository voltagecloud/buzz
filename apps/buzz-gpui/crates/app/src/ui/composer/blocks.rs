//! Keys that edit list and code blocks (`composer-blocks.ts`,
//! `EditableInput.tsx`): Enter inside a list starts the next item and
//! leaves the list from an empty one, Tab and Shift+Tab sink and lift list
//! items, and Backspace at the start of a code block turns it back into
//! text. The composer edits Markdown source, so each is an edit of the
//! line prefixes.

use super::format::Edit;
use std::ops::Range;

/// A list item's line: its indentation, marker and where its text starts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    indent: usize,
    /// `-`, `*`, `+`, or the number and its `.` or `)`.
    marker: String,
    number: Option<u64>,
    /// The byte after the marker's space.
    text: usize,
}

impl Item {
    /// Columns a nested item indents by: up to its parent's text.
    fn width(&self) -> usize {
        self.text - self.indent
    }

    fn next_marker(&self) -> String {
        match self.number {
            Some(number) => {
                let delimiter = self.marker.chars().last().unwrap_or('.');
                format!("{}{delimiter}", number + 1)
            }
            None => self.marker.clone(),
        }
    }
}

fn line_range(text: &str, at: usize) -> Range<usize> {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    start..end
}

fn item(line: &str) -> Option<Item> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let (marker, number) = match rest.bytes().next()? {
        b'-' | b'*' | b'+' => (rest[..1].to_string(), None),
        b'0'..=b'9' => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 9 || !matches!(rest.as_bytes().get(digits), Some(b'.' | b')')) {
                return None;
            }
            (rest[..=digits].to_string(), rest[..digits].parse().ok())
        }
        _ => return None,
    };
    let after = indent + marker.len();
    match line.as_bytes().get(after) {
        Some(b' ') => Some(Item {
            indent,
            marker,
            number,
            text: after + 1,
        }),
        None => Some(Item {
            indent,
            marker,
            number,
            text: after,
        }),
        _ => None,
    }
}

/// Enter in a list item: the next item, or out of the list from an empty
/// one (a nested one moves up a level). `None` outside lists, so Enter
/// sends.
pub fn enter(text: &str, selection: Range<usize>) -> Option<Edit> {
    let line = line_range(text, selection.start);
    if selection.end > line.end {
        return None;
    }
    let item = item(&text[line.clone()])?;
    if text[line.start + item.text.min(line.len())..line.end]
        .trim()
        .is_empty()
    {
        return Some(match lift(line.clone(), &item) {
            Some(edit) => edit,
            None => Edit {
                changes: vec![(line.clone(), String::new())],
                selection: line.start..line.start,
            },
        });
    }
    let caret = selection.start.max(line.start + item.text);
    let insert = format!("\n{}{} ", " ".repeat(item.indent), item.next_marker());
    let end = caret + insert.len();
    let mut changes = vec![(caret..selection.end.max(caret), insert)];
    // Later numbers of this list move up by one.
    if item.number.is_some() {
        changes.extend(renumber(
            text,
            line.end,
            item.indent,
            item.number.unwrap_or(1) + 2,
        ));
    }
    Some(Edit {
        changes,
        selection: end..end,
    })
}

/// Numbers the list items after `from` at `indent` from `first`, until
/// the list ends.
fn renumber(text: &str, from: usize, indent: usize, first: u64) -> Vec<(Range<usize>, String)> {
    let mut changes = Vec::new();
    let mut at = from;
    let mut expected = first;
    while at < text.len() {
        let line = line_range(text, at + 1);
        let Some(next) = item(&text[line.clone()]) else {
            break;
        };
        if next.indent < indent {
            break;
        }
        if next.indent == indent {
            let Some(_) = next.number else {
                break;
            };
            // Numbers keep counting on from the new item.
            let digits = next.marker.len() - 1;
            let start = line.start + next.indent;
            changes.push((start..start + digits, expected.to_string()));
            expected += 1;
        }
        at = line.end;
    }
    changes.retain(|(range, new)| text[range.clone()] != *new);
    changes
}

/// A nested item moves up a level; a top-level one leaves the list.
fn lift(line: Range<usize>, item: &Item) -> Option<Edit> {
    let caret_in_text = line.start + item.text;
    if item.indent == 0 {
        return None;
    }
    let remove = item.indent.min(item.width().max(2));
    Some(Edit {
        changes: vec![(line.start..line.start + remove, String::new())],
        selection: caret_in_text - remove..caret_in_text - remove,
    })
}

/// Tab: the selected items sink one level under the item above. A sunk
/// numbered item starts its own list at 1.
pub fn sink(text: &str, selection: Range<usize>) -> Option<Edit> {
    let lines = lines(text, &selection);
    let items: Vec<(Range<usize>, Item)> = lines
        .iter()
        .filter_map(|line| Some((line.clone(), item(&text[line.clone()])?)))
        .collect();
    if items.len() != lines.len() || items.is_empty() {
        return None;
    }
    // The first item needs an item above it to nest under.
    let first = &items[0];
    if first.0.start == 0 {
        return None;
    }
    let above = line_range(text, first.0.start - 1);
    let parent = item(&text[above.clone()])?;
    if parent.indent < first.1.indent {
        return None;
    }
    let mut changes = Vec::new();
    for (line, item) in &items {
        let width = if *line == first.0 {
            parent.width()
        } else {
            item.width()
        };
        changes.push((line.start..line.start, " ".repeat(width)));
        if item.number.is_some() {
            let start = line.start + item.indent;
            changes.push((start..start + item.marker.len() - 1, "1".into()));
        }
    }
    changes.retain(|(range, new)| text[range.clone()] != *new);
    Some(finish_lines(changes, selection))
}

/// Shift+Tab: the selected items lift one level, out of the list from the
/// top level.
pub fn lift_items(text: &str, selection: Range<usize>) -> Option<Edit> {
    let lines = lines(text, &selection);
    let mut changes = Vec::new();
    for line in &lines {
        let item = item(&text[line.clone()])?;
        if item.indent > 0 {
            let remove = item.indent.min(item.width().max(2));
            changes.push((line.start..line.start + remove, String::new()));
        } else {
            changes.push((
                line.start..line.start + item.text.min(line.len()),
                String::new(),
            ));
        }
    }
    Some(finish_lines(changes, selection))
}

fn lines(text: &str, selection: &Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = selection.start;
    loop {
        let line = line_range(text, at);
        let end = line.end;
        out.push(line);
        if end >= selection.end || end >= text.len() {
            break;
        }
        at = end + 1;
    }
    out
}

fn finish_lines(mut changes: Vec<(Range<usize>, String)>, selection: Range<usize>) -> Edit {
    changes.sort_by_key(|(range, _)| range.start);
    let map = |offset: usize| {
        let mut out = offset as isize;
        for (range, text) in &changes {
            if range.start < offset {
                let removed = range.end.min(offset) - range.start;
                out += text.len() as isize - removed as isize;
            }
        }
        out.max(0) as usize
    };
    Edit {
        selection: map(selection.start)..map(selection.end),
        changes,
    }
}

/// Backspace at the start of a code block's first line removes its
/// fences, so its text is ordinary text again.
pub fn backspace(text: &str, selection: Range<usize>) -> Option<Edit> {
    if !selection.is_empty() {
        return None;
    }
    let line = line_range(text, selection.start);
    if selection.start != line.start || line.start == 0 {
        return None;
    }
    let fence = line_range(text, line.start - 1);
    let opening = text[fence.clone()].trim_start();
    if !(opening.starts_with("```") || opening.starts_with("~~~")) {
        return None;
    }
    // It must open a block: an even number of fences above it.
    let fences_above = text[..fence.start]
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            l.starts_with("```") || l.starts_with("~~~")
        })
        .count();
    if fences_above % 2 == 1 {
        return None;
    }
    let mark = &opening[..3];
    let mut changes = vec![(fence.start..line.start, String::new())];
    let mut at = line.end;
    while at < text.len() {
        let next = line_range(text, at + 1);
        if text[next.clone()].trim_start().starts_with(mark) {
            changes.push((at..next.end, String::new()));
            break;
        }
        at = next.end;
    }
    let caret = fence.start;
    Some(Edit {
        changes,
        selection: caret..caret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(edit: Option<Edit>, text: &str) -> Option<(String, Range<usize>)> {
        let edit = edit?;
        let mut out = text.to_string();
        let mut changes = edit.changes.clone();
        changes.sort_by_key(|(range, _)| range.start);
        for (range, replacement) in changes.iter().rev() {
            out.replace_range(range.clone(), replacement);
        }
        Some((out, edit.selection))
    }

    #[test]
    fn enter_continues_and_leaves_lists() {
        let text = "- one";
        assert_eq!(
            run(enter(text, 5..5), text),
            Some(("- one\n- ".into(), 8..8))
        );
        let text = "1. one\n2. two";
        assert_eq!(
            run(enter(text, 6..6), text),
            Some(("1. one\n2. \n3. two".into(), 10..10))
        );
        // An empty item leaves the list; a nested one moves up a level.
        let text = "- one\n- ";
        assert_eq!(run(enter(text, 8..8), text), Some(("- one\n".into(), 6..6)));
        let text = "- one\n  - ";
        assert_eq!(
            run(enter(text, 10..10), text),
            Some(("- one\n- ".into(), 8..8))
        );
        assert!(enter("plain", 5..5).is_none());
        assert!(enter("> quote", 7..7).is_none());
    }

    #[test]
    fn tab_sinks_and_shift_tab_lifts() {
        let text = "- one\n- two";
        assert_eq!(
            run(sink(text, 11..11), text),
            Some(("- one\n  - two".into(), 13..13))
        );
        // The first item has nothing to nest under.
        assert!(sink(text, 3..3).is_none());
        let text = "1. one\n2. two";
        assert_eq!(
            run(sink(text, 13..13), text),
            Some(("1. one\n   1. two".into(), 16..16))
        );
        let text = "- one\n  - two";
        assert_eq!(
            run(lift_items(text, 13..13), text),
            Some(("- one\n- two".into(), 11..11))
        );
        let text = "- one";
        assert_eq!(
            run(lift_items(text, 5..5), text),
            Some(("one".into(), 3..3))
        );
        assert!(lift_items("plain", 2..2).is_none());
    }

    #[test]
    fn backspace_leaves_a_code_block() {
        let text = "intro\n```\ncode\n```\nafter";
        assert_eq!(
            run(backspace(text, 10..10), text),
            Some(("intro\ncode\nafter".into(), 6..6))
        );
        assert!(backspace(text, 11..11).is_none());
        assert!(backspace("a\nb", 2..2).is_none());
    }
}
