//! Copy, cut and paste in the composer (`composer-clipboard.ts`,
//! `EditableInput.tsx`). The text flavour is the Markdown as a reader
//! expects it, with chips as their `@Name` and `#channel` labels. GPUI's
//! Linux clipboard holds text only, so the locators ride along as the
//! string's metadata, which this app reads back when it pastes its own
//! copy, as buzz-app reads its `data-buzz-copy` HTML.

use gpui_kit::component::input::InputContent;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// The metadata of a composer copy.
#[derive(Serialize, Deserialize)]
pub struct Copied {
    /// The Markdown with every identity and channel link.
    pub buzz_composer: String,
}

/// The two flavours of the selected part of `content`.
pub fn copy(content: &InputContent, selection: Range<usize>) -> (String, Copied) {
    let text = content.text();
    let mut plain = String::new();
    let mut markdown = String::new();
    let mut at = selection.start;
    for span in content.tokens() {
        let range = span.range();
        if range.end <= selection.start || range.start >= selection.end {
            continue;
        }
        let start = range.start.max(at);
        plain.push_str(&text[at..start.max(at)]);
        markdown.push_str(&text[at..start.max(at)]);
        let token = span.token();
        let source = token.text().to_string();
        if let Some(name) = source.strip_prefix('@') {
            // A recipient: its name, and an identity link that pasting here
            // can make a recipient again.
            plain.push_str(&source);
            markdown.push_str(
                &crate::ui::message_edit::mention_text(token.id(), name)
                    .unwrap_or_else(|| source.clone()),
            );
        } else {
            plain.push_str(token.label());
            markdown.push_str(&source);
        }
        at = range.end.min(selection.end).max(at);
    }
    plain.push_str(&text[at.min(selection.end)..selection.end]);
    markdown.push_str(&text[at.min(selection.end)..selection.end]);
    (
        plain,
        Copied {
            buzz_composer: markdown,
        },
    )
}

/// Whether `text` is one link and nothing else, which pastes with a
/// trailing space (`messageLinkParts`).
pub fn lone_link(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == text
        && !trimmed.contains(char::is_whitespace)
        && (trimmed.starts_with("https://")
            || trimmed.starts_with("http://")
            || trimmed.starts_with("buzz://"))
        && url::Url::parse(trimmed).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui_kit::component::input::InlineToken;

    #[test]
    fn copies_chips_as_their_names_and_keeps_locators() {
        let pubkey = "200f548dc52579907f8384eba9daaa09e485bb5cdb6f4c62c5c69a48d92f0ddb";
        let text = "hi @Alice and [\\#general](buzz://channel/abc) **now**";
        let content = InputContent::new(text)
            .with_token(3..9, InlineToken::new(pubkey, "@Alice"))
            .unwrap()
            .with_token(
                14..45,
                InlineToken::new("abc", &text[14..45]).with_label("#general"),
            )
            .unwrap();
        let (plain, copied) = copy(&content, 0..text.len());
        assert_eq!(plain, "hi @Alice and #general **now**");
        assert!(
            copied
                .buzz_composer
                .starts_with("hi [\\@Alice](nostr:npub1")
        );
        assert!(
            copied
                .buzz_composer
                .ends_with("and [\\#general](buzz://channel/abc) **now**")
        );
        // A partial selection copies what it covers.
        assert_eq!(copy(&content, 0..2).0, "hi");
    }

    #[test]
    fn spots_a_lone_link() {
        assert!(lone_link("https://buzz.dev/a"));
        assert!(!lone_link("see https://buzz.dev"));
        assert!(!lone_link(" https://buzz.dev"));
        assert!(!lone_link("buzz.dev"));
    }
}
