//! What "Copy message" and "Copy link" put on the clipboard
//! (`message-copy.ts`).

use super::identity::Names;
use super::markdown;
use buzz_relay_client::Profiles;
use buzz_relay_client::fold::{MessageRow, message_markdown};
use nostr::nips::nip19::ToBech32;

/// `messageCopyLink`: `buzz://message?channel&id`, with `thread` for a
/// reply. `None` until the relay has the message.
pub fn link(row: &MessageRow) -> Option<String> {
    if !super::message::delivered(row) {
        return None;
    }
    let mut url = url::Url::parse("buzz://message").expect("static URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("channel", &row.channel)
            .append_pair("id", &row.id.to_hex());
        if let Some(root) = row.thread_root_id {
            query.append_pair("thread", &root.to_hex());
        }
    }
    Some(url.to_string())
}

/// `messageCopyText`: the body with bound mentions as `[@Name](nostr:npub…)`
/// links (never inside code or other literal syntax), then each
/// attachment's URL, blank lines between.
pub fn text(row: &MessageRow, profiles: &Profiles, names: &Names) -> String {
    let mut content = row.content.clone();
    if !row.edited
        && let Some(tree) = message_markdown(&content)
    {
        let mut literals = Vec::new();
        markdown::literal_ranges(&tree, true, &mut literals);
        let labels = markdown::mention_labels(row, profiles, names);
        content = markdown::bind_mentions(
            &row.content,
            &labels,
            &literals,
            &row.attachment_seams,
            |label, key, out| {
                out.push('[');
                for c in label.chars() {
                    if matches!(c, '\\' | '[' | ']') {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push_str("](nostr:");
                out.push_str(&key.to_bech32().expect("keys encode"));
                out.push(')');
            },
        );
    }
    std::iter::once(content.as_str())
        .chain(row.attachments.iter().map(|a| a.url.as_str()))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::identity::{self, AgentLibrary};
    use buzz_relay_client::fold::{Attachment, AttachmentKind, Delivery, LocalDelivery};
    use buzz_relay_client::{EventId, Profile};
    use core::prelude::v1::test;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn copies_the_body_then_attachment_urls() {
        let mut row = super::super::markdown::tests::row("Look");
        let names = identity::names(
            &Profiles::default(),
            &AgentLibrary::default(),
            "",
            None,
            None,
        );
        assert_eq!(text(&row, &Profiles::default(), &names), "Look");
        row.attachments = vec![Attachment {
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
        assert_eq!(
            text(&row, &Profiles::default(), &names),
            "Look\n\nhttps://x.test/media/a.png"
        );
    }

    #[test]
    fn bound_mentions_copy_as_identity_links_outside_code() {
        let sam = buzz_identity::Identity::ephemeral().public_key();
        let profiles: Profiles = Arc::new(HashMap::from([(
            sam.to_hex(),
            Arc::new(Profile {
                name: "Sam".into(),
                ..Default::default()
            }),
        )]));
        let names = identity::names(&profiles, &AgentLibrary::default(), "", None, None);
        let mut row = super::super::markdown::tests::row("hi @Sam and `@Sam`");
        row.mentions = vec![sam];
        let npub = sam.to_bech32().unwrap();
        assert_eq!(
            text(&row, &profiles, &names),
            format!("hi [@Sam](nostr:{npub}) and `@Sam`")
        );
    }

    #[test]
    fn links_keep_the_thread_for_replies() {
        let mut row = super::super::markdown::tests::row("x");
        let root = EventId::from_byte_array([4; 32]);
        row.thread_root_id = Some(root);
        let link = link(&row).unwrap();
        assert!(link.contains(&format!("thread={}", root.to_hex())));
        row.delivery = Some(LocalDelivery {
            status: Delivery::Sending,
            error: None,
        });
        assert_eq!(super::link(&row), None);
    }
}
