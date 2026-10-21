//! Editing a message in the composer (`message-edit.ts`, `useMessageEdit.ts`):
//! the text it starts from, the checks a save must pass, and the outbox
//! state the composer shows while the edit is delivered.

use super::identity::Names;
use super::markdown;
use buzz_relay_client::Profiles;
use buzz_relay_client::fold::{MessageRow, message_markdown};
use nostr::nips::nip19::ToBech32;

pub const UNAVAILABLE: &str = "This message is no longer available to edit.";
pub const CONFLICT: &str =
    "This message changed while you were editing. Copy your changes, then cancel and reopen it.";
pub const ATTACHMENTS_CHANGED: &str =
    "Keep attachment links unchanged. Only message text can be edited here.";
pub const ATTACHMENTS_EMPTY: &str =
    "Keep attachment links unchanged. To remove this message, use Delete message.";
pub const BUSY: &str = "Finish or cancel your current edit first.";

/// How the composer shows an edit being delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditStatus {
    /// Submitted: the text is locked until the edit lands or is closed.
    pub locked: bool,
    /// Still sending: the edit cannot be closed.
    pub busy: bool,
    /// Failed or unconfirmed: "Retry edit".
    pub retryable: bool,
}

/// The row's source text, before attachment projection.
pub fn source(row: &MessageRow) -> &str {
    row.source_content.as_deref().unwrap_or(&row.content)
}

/// `profileMentionLink`: a literal one-line label with every ASCII
/// punctuation mark escaped, linking an exact identity.
pub fn profile_link(label: &str, target: &str) -> String {
    let mut text = String::new();
    let mut last_newline = false;
    for c in label.chars() {
        if c == '\r' || c == '\n' {
            if !last_newline {
                text.push(' ');
            }
            last_newline = true;
            continue;
        }
        last_newline = false;
        if c.is_ascii_punctuation() {
            text.push('\\');
        }
        text.push(c);
    }
    format!("[{text}]({target})")
}

/// A picked mention while editing: an identity link, not a recipient.
pub fn mention_text(pubkey: &str, name: &str) -> Option<String> {
    let npub = nostr::prelude::PublicKey::from_hex(pubkey)
        .ok()?
        .to_bech32()
        .ok()?;
    Some(profile_link(&format!("@{name}"), &format!("nostr:{npub}")))
}

/// `messageEditText`: the source with its attachment Markdown, already
/// bound names turned into explicit identity links. Edited prose stays
/// unbound; an edit never invents recipients.
pub fn edit_text(row: &MessageRow, profiles: &Profiles, names: &Names) -> String {
    let text = source(row);
    if row.edited {
        return text.to_string();
    }
    let Some(tree) = message_markdown(text) else {
        return text.to_string();
    };
    let mut literals = Vec::new();
    markdown::literal_ranges(&tree, true, &mut literals);
    let labels = markdown::mention_labels(row, profiles, names);
    // The source keeps its attachment Markdown, so it has no seams.
    markdown::bind_mentions(text, &labels, &literals, &[], |label, key, out| {
        let npub = key.to_bech32().expect("keys encode");
        out.push_str(&profile_link(label, &format!("nostr:{npub}")));
    })
}

/// `(label, url)` of Markdown links and images whose destination is one
/// of `urls`, in order.
fn attachment_links<'a>(text: &'a str, urls: &[&str]) -> Vec<(&'a str, &'a str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = text[from..].find('[').map(|i| from + i) {
        from = open + 1;
        let Some(close) = text[open..].find("](").map(|i| open + i) else {
            break;
        };
        let start = close + 2;
        let Some(end) = text[start..].find(')').map(|i| start + i) else {
            break;
        };
        let url = &text[start..end];
        if urls.contains(&url) {
            found.push((&text[open + 1..close], url));
            from = end + 1;
        }
    }
    found
}

/// `validateMessageEdit`: attachment links must stay as they were.
pub fn validate(row: &MessageRow, initial: &str, content: &str) -> Result<(), &'static str> {
    let urls: Vec<&str> = row.attachments.iter().map(|a| a.url.as_str()).collect();
    let counts_match = urls
        .iter()
        .all(|url| initial.matches(url).count() == content.matches(url).count());
    if !counts_match || attachment_links(initial, &urls) != attachment_links(content, &urls) {
        return Err(ATTACHMENTS_CHANGED);
    }
    Ok(())
}

/// What saving an edit should do, before it is sent (`useMessageEdit`'s
/// `save` and `MessageComposer`'s `send`).
#[derive(Debug, PartialEq)]
pub enum Save {
    /// Nothing changed: close the edit.
    Close,
    /// Empty text: ask to delete the message instead.
    Delete,
    Error(&'static str),
    Send(String),
}

pub fn plan(
    target: &MessageRow,
    current: Option<&MessageRow>,
    current_editable: bool,
    initial: &str,
    body: &str,
) -> Save {
    if body.trim().is_empty() {
        return if target.attachments.is_empty() {
            Save::Delete
        } else {
            Save::Error(ATTACHMENTS_EMPTY)
        };
    }
    let Some(current) = current.filter(|_| current_editable) else {
        return Save::Error(UNAVAILABLE);
    };
    if source(current) != source(target) {
        return Save::Error(CONFLICT);
    }
    if body == initial || body == source(target) {
        return Save::Close;
    }
    match validate(target, source(target), body) {
        Ok(()) => Save::Send(body.to_string()),
        Err(error) => Save::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::identity::{self, AgentLibrary};
    use buzz_relay_client::Profile;
    use buzz_relay_client::fold::{Attachment, AttachmentKind};
    use core::prelude::v1::test;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn row(text: &str) -> MessageRow {
        super::super::markdown::tests::row(text)
    }

    #[test]
    fn bound_mentions_become_identity_links() {
        let sam = buzz_identity::Identity::ephemeral().public_key();
        let profiles: Profiles = Arc::new(HashMap::from([(
            sam.to_hex(),
            Arc::new(Profile {
                name: "Sam R.".into(),
                ..Default::default()
            }),
        )]));
        let names = identity::names(&profiles, &AgentLibrary::default(), "", None, None);
        let mut message = row("hi @Sam R. and `@Sam R.`");
        message.mentions = vec![sam];
        let npub = sam.to_bech32().unwrap();
        assert_eq!(
            edit_text(&message, &profiles, &names),
            format!("hi [\\@Sam R\\.](nostr:{npub}) and `@Sam R.`")
        );
        // Code spans are found as rendering finds them, not by counting
        // backticks.
        let mut spans = row("``it`s`` for @Sam R.");
        spans.mentions = vec![sam];
        assert_eq!(
            edit_text(&spans, &profiles, &names),
            format!("``it`s`` for [\\@Sam R\\.](nostr:{npub})")
        );
        message.edited = true;
        assert_eq!(
            edit_text(&message, &profiles, &names),
            "hi @Sam R. and `@Sam R.`"
        );
    }

    #[test]
    fn saves_follow_the_reference() {
        let target = row("hello");
        assert_eq!(
            plan(&target, Some(&target), true, "hello", "hello"),
            Save::Close
        );
        assert_eq!(
            plan(&target, Some(&target), true, "hello", "  "),
            Save::Delete
        );
        assert_eq!(
            plan(&target, None, true, "hello", "bye"),
            Save::Error(UNAVAILABLE)
        );
        let changed = row("hello there");
        assert_eq!(
            plan(&target, Some(&changed), true, "hello", "bye"),
            Save::Error(CONFLICT)
        );
        assert_eq!(
            plan(&target, Some(&target), true, "hello", "bye"),
            Save::Send("bye".into())
        );
    }

    #[test]
    fn attachment_links_must_stay() {
        let mut target = row("look ![a.png](https://x.test/media/a.png)");
        target.attachments = vec![Attachment {
            url: "https://x.test/media/a.png".into(),
            kind: AttachmentKind::Image,
            mime: None,
            size: None,
            name: None,
            duration: None,
            waveform: None,
            voice_note: false,
            dimensions: None,
            blurhash: None,
            preview_url: None,
            insecure: false,
        }];
        target.source_content = Some(target.content.clone());
        let initial = target.content.clone();
        assert_eq!(
            plan(&target, Some(&target), true, &initial, "  "),
            Save::Error(ATTACHMENTS_EMPTY)
        );
        assert_eq!(
            plan(&target, Some(&target), true, &initial, "look"),
            Save::Error(ATTACHMENTS_CHANGED)
        );
        assert!(matches!(
            plan(
                &target,
                Some(&target),
                true,
                &initial,
                "see ![a.png](https://x.test/media/a.png)"
            ),
            Save::Send(_)
        ));
        assert_eq!(profile_link("@A*b", "nostr:x"), "[\\@A\\*b](nostr:x)");
    }
}
