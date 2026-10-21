//! Who a message for agents opens by addressing (`audience-address.ts`).

use super::identity::Names;
use super::markdown;
use buzz_relay_client::fold::{MessageRow, message_markdown};
use buzz_relay_client::{Profiles, PublicKey};
use std::sync::Arc;

/// A run of a body: plain text, or a mention bound to one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part<'a> {
    pub text: &'a str,
    pub target: Option<PublicKey>,
}

/// `profileMentionParts`: the body split at mentions bound to exactly one
/// signed recipient by its exact known name. Display only; an edited body
/// binds none, and code or other literal syntax never binds.
pub fn mention_parts<'a>(row: &'a MessageRow, profiles: &Profiles, names: &Names) -> Vec<Part<'a>> {
    let text = row.content.as_str();
    let whole = || vec![Part { text, target: None }];
    if row.edited {
        return whole();
    }
    let labels = markdown::mention_labels(row, profiles, names);
    let Some(tree) = (!labels.is_empty())
        .then(|| message_markdown(text))
        .flatten()
    else {
        return whole();
    };
    let mut literals = Vec::new();
    markdown::literal_ranges(&tree, true, &mut literals);
    let seams = &row.attachment_seams;
    let mut parts = Vec::new();
    let (mut plain, mut i) = (0, 0);
    while i < text.len() {
        let Some((end, keys)) = markdown::mention_at(text, i, &labels, |start, end| {
            seams.iter().any(|seam| start < *seam && *seam < end)
        }) else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        // A legacy namesake qualifier names someone else.
        let qualified = text[end..].strip_prefix(" (").is_some_and(|q| {
            q.len() >= 65
                && q.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
                && q.as_bytes()[64] == b')'
        });
        let literal = literals.iter().any(|&(s, e)| i < e && end > s);
        if let [key] = keys
            && !qualified
            && !literal
        {
            if i > plain {
                parts.push(Part {
                    text: &text[plain..i],
                    target: None,
                });
            }
            parts.push(Part {
                text: &text[i..end],
                target: Some(*key),
            });
            plain = end;
        }
        i = end;
    }
    if plain < text.len() {
        parts.push(Part {
            text: &text[plain..],
            target: None,
        });
    }
    if parts.is_empty() { whole() } else { parts }
}

/// Who a message opens by addressing, and the message as shown without
/// them.
#[derive(Debug, Clone, PartialEq)]
pub struct Addressed {
    pub recipients: Vec<PublicKey>,
    pub body: Arc<MessageRow>,
}

/// `addressedMessage`: the leading bound mentions, with the spaces, commas
/// and colons between them, become recipients and leave the body; its
/// attachment seams move with it. Display only: a message that is nothing
/// but mentions, or opens with prose, is shown as written.
pub fn addressed_message(row: &Arc<MessageRow>, parts: &[Part]) -> Addressed {
    let mut recipients: Vec<PublicKey> = Vec::new();
    let mut cut = 0;
    for part in parts {
        if let Some(key) = part.target {
            if !recipients.contains(&key) {
                recipients.push(key);
            }
            cut += part.text.len();
            continue;
        }
        let gap = part.text.len() - part.text.trim_start_matches([' ', '\t', ',', ':']).len();
        cut += gap;
        if gap < part.text.len() {
            break;
        }
    }
    if recipients.is_empty() || row.content[cut..].trim().is_empty() {
        return Addressed {
            recipients: Vec::new(),
            body: row.clone(),
        };
    }
    let body = MessageRow {
        content: row.content[cut..].to_owned(),
        attachment_seams: row
            .attachment_seams
            .iter()
            .filter(|seam| **seam > cut)
            .map(|seam| seam - cut)
            .collect(),
        ..(**row).clone()
    };
    Addressed {
        recipients,
        body: Arc::new(body),
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::{self, AgentLibrary};
    use super::super::markdown::tests::row;
    use super::*;
    use buzz_relay_client::Profile;
    use std::collections::HashMap;

    fn key() -> PublicKey {
        buzz_identity::Identity::ephemeral().public_key()
    }

    fn message(content: &str, mentions: &[PublicKey], seams: &[usize]) -> Arc<MessageRow> {
        let mut row = row(content);
        row.mentions = mentions.to_vec();
        row.attachment_seams = seams.to_vec();
        Arc::new(row)
    }

    fn at(key: PublicKey, text: &str) -> Part<'_> {
        Part {
            text,
            target: Some(key),
        }
    }

    fn plain(text: &str) -> Part<'_> {
        Part { text, target: None }
    }

    #[test]
    fn takes_every_leading_mention_as_a_recipient_and_shifts_seams_past_them() {
        let (rivet, bob) = (key(), key());
        let row = message("@Rivet, @Bob: run it", &[rivet, bob], &[3, 15]);
        let addressed = addressed_message(
            &row,
            &[
                at(rivet, "@Rivet"),
                plain(", "),
                at(bob, "@Bob"),
                plain(": run it"),
            ],
        );
        assert_eq!(addressed.recipients, vec![rivet, bob]);
        assert_eq!(addressed.body.content, "run it");
        assert_eq!(addressed.body.attachment_seams, vec![1]);
    }

    #[test]
    fn keeps_the_line_break_and_indentation_after_a_leading_mention() {
        let rivet = key();
        let row = message("@Rivet\n    code", &[rivet], &[]);
        let addressed = addressed_message(&row, &[at(rivet, "@Rivet"), plain("\n    code")]);
        assert_eq!(addressed.recipients, vec![rivet]);
        assert_eq!(addressed.body.content, "\n    code");
    }

    #[test]
    fn a_message_that_opens_with_prose_or_is_only_mentions_is_shown_as_written() {
        let rivet = key();
        for parts in [
            vec![plain("thanks "), at(rivet, "@Rivet")],
            vec![at(rivet, "@Rivet")],
        ] {
            let content: String = parts.iter().map(|part| part.text).collect();
            let row = message(&content, &[rivet], &[]);
            let addressed = addressed_message(&row, &parts);
            assert!(addressed.recipients.is_empty());
            assert!(Arc::ptr_eq(&addressed.body, &row));
        }
    }

    #[test]
    fn binds_known_names_of_signed_recipients_outside_code() {
        let rivet = key();
        let profiles: Profiles = Arc::new(HashMap::from([(
            rivet.to_hex(),
            Arc::new(Profile {
                name: "Rivet".into(),
                ..Default::default()
            }),
        )]));
        let names = identity::names(&profiles, &AgentLibrary::default(), "", None, None);
        let row = message("@Rivet, **run** `@Rivet` then @Rivetx", &[rivet], &[]);
        assert_eq!(
            mention_parts(&row, &profiles, &names),
            vec![
                at(rivet, "@Rivet"),
                plain(", **run** `@Rivet` then @Rivetx")
            ]
        );
        let mut edited = (*row).clone();
        edited.edited = true;
        assert_eq!(
            mention_parts(&edited, &profiles, &names),
            vec![plain(&row.content)]
        );
    }
}
