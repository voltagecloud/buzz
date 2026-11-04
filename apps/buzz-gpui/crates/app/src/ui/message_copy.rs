//! What "Copy message", "Copy link" and "Copy thread" put on the
//! clipboard (`message-copy.ts`).

use super::identity::Names;
use super::markdown;
use buzz_relay_client::fold::{MessageRow, message_markdown};
use buzz_relay_client::{EventId, Profiles};
use nostr::nips::nip19::ToBech32;
use std::collections::HashMap;
use std::sync::Arc;

/// `messageCopyLink`: `buzz://message?channel&id`, with `thread` for a
/// reply. `None` until the relay has the message.
pub fn link(row: &MessageRow) -> Option<String> {
    if !super::message::delivered(row) {
        return None;
    }
    Some(message_link(&row.channel, row.id, row.thread_root_id))
}

fn message_link(channel: &str, id: EventId, thread: Option<EventId>) -> String {
    let mut url = url::Url::parse("buzz://message").expect("static URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("channel", channel)
            .append_pair("id", &id.to_hex());
        if let Some(root) = thread {
            query.append_pair("thread", &root.to_hex());
        }
    }
    url.to_string()
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

/// What a thread panel has loaded, for [`thread`].
pub struct LoadedThread<'a> {
    /// The conversation's id, for the root's link.
    pub channel: &'a str,
    /// `#name` for a channel, or how else to name the conversation.
    pub place: Option<String>,
    pub root_id: EventId,
    /// `None` when the original message is unavailable.
    pub root: Option<&'a MessageRow>,
    pub replies: &'a [Arc<MessageRow>],
    /// Lines the panel shows about the thread as a whole: replies not
    /// loaded, and banners such as a remote session's state.
    pub notes: Vec<String>,
    /// Each agent's status on a message, as its activity bubble shows it:
    /// the agent's key and the status line.
    pub activity: HashMap<EventId, Vec<(String, String)>>,
}

/// "Copy thread": Markdown of what the viewer sees in a thread. A heading
/// with the root's link, the thread's notes, then every loaded message in
/// time order with its author, UTC time, body and attachment URLs as
/// written, and the agents' status on it.
pub fn thread(loaded: &LoadedThread, names: &Names) -> String {
    let mut out = String::from("# Thread");
    if let Some(place) = &loaded.place {
        out.push_str(" in ");
        out.push_str(place);
    }
    // An unsent root has no link yet.
    if loaded.root.is_none_or(super::message::delivered) {
        out.push_str(" · ");
        out.push_str(&message_link(loaded.channel, loaded.root_id, None));
    }
    out.push('\n');
    let mut notes = loaded.notes.clone();
    if loaded.root.is_none() {
        notes.insert(0, "The original message is unavailable.".into());
    }
    if !notes.is_empty() {
        out.push('\n');
        for note in &notes {
            out.push_str(&quote(note));
        }
    }
    let mut replies: Vec<&MessageRow> = loaded.replies.iter().map(Arc::as_ref).collect();
    replies.sort_by_key(|row| (row.created_at_ms, row.id));
    for row in loaded.root.into_iter().chain(replies) {
        // Join and leave rows carry no body.
        if row.membership.is_some() {
            continue;
        }
        out.push('\n');
        out.push_str(&byline(row, names));
        out.push('\n');
        let body = std::iter::once(row.content.as_str())
            .chain(row.attachments.iter().map(|a| a.url.as_str()))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !body.is_empty() {
            out.push_str(&body);
            out.push('\n');
        }
        for (agent, status) in loaded.activity.get(&row.id).into_iter().flatten() {
            out.push('\n');
            out.push_str(&quote(&format!(
                "Activity: {} — {status}",
                names.display(agent)
            )));
        }
    }
    out
}

/// `**Name** (agent) · 2026-10-29 11:22:30 UTC`, and whether it was sent.
fn byline(row: &MessageRow, names: &Names) -> String {
    let author = row.author.to_hex();
    let name = names.display(&author);
    let mut line = format!("**{}**", escape(&name));
    if (row.is_agent_envelope() || names.is_agent(&author)) && !name.ends_with("(agent)") {
        line.push_str(" (agent)");
    }
    line.push_str(" · ");
    line.push_str(&utc(row.created_at_ms));
    if !super::message::delivered(row) {
        line.push_str(" · not sent");
    }
    line
}

fn utc(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "unknown time".into(),
            |at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        )
}

/// A name inside `**…**`.
fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A blockquote, every line marked.
fn quote(text: &str) -> String {
    text.lines().map(|line| format!("> {line}\n")).collect()
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

    fn person(name: &str, agent: bool) -> (nostr::prelude::PublicKey, Arc<Profile>) {
        let key = buzz_identity::Identity::ephemeral().public_key();
        let profile = Arc::new(Profile {
            name: name.into(),
            is_agent: agent,
            ..Default::default()
        });
        (key, profile)
    }

    #[test]
    fn copies_a_thread_in_time_order_with_its_status() {
        let (blu, blu_profile) = person("blu", false);
        let (augustus, augustus_profile) = person("Augustus", true);
        let profiles: Profiles = Arc::new(HashMap::from([
            (blu.to_hex(), blu_profile),
            (augustus.to_hex(), augustus_profile),
        ]));
        let names = identity::names(&profiles, &AgentLibrary::default(), "", None, None);
        let mut root = super::super::markdown::tests::row("@Augustus ping");
        root.channel = Arc::from("chan");
        root.author = blu;
        root.created_at_ms = 1_793_272_950_000;
        let reply = |byte: u8, text: &str, at: u64| {
            let mut row = super::super::markdown::tests::row(text);
            row.id = EventId::from_byte_array([byte; 32]);
            row.author = augustus;
            row.thread_root_id = Some(root.id);
            row.created_at_ms = at;
            Arc::new(row)
        };
        let later = reply(3, "done", 1_793_273_100_000);
        let mut first = (*reply(2, "@blu pong", 1_793_273_022_000)).clone();
        first.attachments = vec![Attachment {
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
        // Rendered order is by ancestry; the copy is by time.
        let replies = [later.clone(), Arc::new(first)];
        let loaded = LoadedThread {
            channel: "chan",
            place: Some("#general".into()),
            root_id: root.id,
            root: Some(&root),
            replies: &replies,
            notes: vec![
                "Older replies were not loaded.".into(),
                "The session has ended.".into(),
            ],
            activity: HashMap::from([(
                later.id,
                vec![(
                    augustus.to_hex(),
                    "Could not finish: sandbox restarted".into(),
                )],
            )]),
        };
        let root_hex = root.id.to_hex();
        assert_eq!(
            thread(&loaded, &names),
            format!(
                "# Thread in #general · buzz://message?channel=chan&id={root_hex}\n\
                 \n\
                 > Older replies were not loaded.\n\
                 > The session has ended.\n\
                 \n\
                 **blu** · 2026-10-29 11:22:30 UTC\n\
                 @Augustus ping\n\
                 \n\
                 **Augustus** (agent) · 2026-10-29 11:23:42 UTC\n\
                 @blu pong\n\
                 \n\
                 https://x.test/media/a.png\n\
                 \n\
                 **Augustus** (agent) · 2026-10-29 11:25:00 UTC\n\
                 done\n\
                 \n\
                 > Activity: Augustus — Could not finish: sandbox restarted\n"
            )
        );
    }

    #[test]
    fn an_unavailable_root_still_links_the_thread() {
        let names = identity::names(
            &Profiles::default(),
            &AgentLibrary::default(),
            "",
            None,
            None,
        );
        let root_id = EventId::from_byte_array([9; 32]);
        let mut pending = super::super::markdown::tests::row("on its way");
        pending.created_at_ms = 0;
        pending.delivery = Some(LocalDelivery {
            status: Delivery::Sending,
            error: None,
        });
        let replies = [Arc::new(pending)];
        let copied = thread(
            &LoadedThread {
                channel: "chan",
                place: None,
                root_id,
                root: None,
                replies: &replies,
                notes: Vec::new(),
                activity: HashMap::new(),
            },
            &names,
        );
        let mut lines = copied.lines();
        assert_eq!(
            lines.next(),
            Some(
                format!(
                    "# Thread · buzz://message?channel=chan&id={}",
                    root_id.to_hex()
                )
                .as_str()
            )
        );
        assert!(copied.contains("> The original message is unavailable.\n"));
        assert!(copied.contains(" · 1970-01-01 00:00:00 UTC · not sent\non its way\n"));
    }

    #[test]
    fn names_cannot_break_the_bold_author() {
        assert_eq!(escape("a*b_c"), "a\\*b\\_c");
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
