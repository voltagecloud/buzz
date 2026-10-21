//! Product feedback (`product-feedback.ts`, `prepare-feedback.ts`): kind
//! 42000 goes to the deployment's private feedback inbox. It carries no
//! channel tag, never enters a view and is never confirmed by echo; the
//! outbox keeps it until the user clears it.

use crate::messages::{Attachment, Draft, attachment_message};
use crate::origin::RelayOrigin;
use crate::outbox::MAX_INPUT_BYTES;
use crate::upload::{UploadFile, sniff_image};
use serde_json::json;

pub const PRODUCT_FEEDBACK_KIND: u16 = 42000;
/// The relay's cap on the serialized tag list.
pub const MAX_TAG_BYTES: usize = 64 << 10;
pub const INVALID: &str = "Feedback must contain text and fit within relay limits.";
pub const NOT_AN_IMAGE: &str = "Choose a PNG, JPEG, GIF, or WebP image.";
pub const IMAGE_TOO_LARGE: &str = "Feedback image exceeds the supported size.";
pub const DIAGNOSTICS_NAME: &str = "feedback-diagnostics.txt";

const MIB: usize = 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 50 * MIB;
const MAX_GIF_BYTES: usize = 10 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Bug,
    Praise,
    NeedsWork,
}

impl Category {
    pub const ALL: [Self; 3] = [Self::Bug, Self::Praise, Self::NeedsWork];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Praise => "praise",
            Self::NeedsWork => "needs-work",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bug => "Bug",
            Self::Praise => "Praise",
            Self::NeedsWork => "Needs work",
        }
    }
}

/// The feedback event: the message with one link per attachment, an
/// optional `category` tag and the attachments' `imeta` tags. Refused
/// when the text is blank or the event would not fit the outbox input
/// budget or the relay's tag cap.
pub fn event(
    message: &str,
    category: Option<Category>,
    attachments: &[Attachment],
    origin: &RelayOrigin,
) -> Result<Draft, String> {
    let (content, attachment_tags) = if attachments.is_empty() {
        (message.trim().to_owned(), Vec::new())
    } else {
        attachment_message(message, attachments, origin).map_err(|error| error.to_string())?
    };
    let mut tags: Vec<Vec<String>> = category
        .map(|category| vec!["category".to_owned(), category.as_str().to_owned()])
        .into_iter()
        .collect();
    tags.extend(attachment_tags);
    let input = json!({
        "kind": PRODUCT_FEEDBACK_KIND,
        "content": content,
        "tags": tags,
    })
    .to_string();
    let tag_bytes = serde_json::to_string(&tags).map_or(usize::MAX, |json| json.len());
    if message.trim().is_empty()
        || content.is_empty()
        || input.len() > MAX_INPUT_BYTES
        || tag_bytes > MAX_TAG_BYTES
    {
        return Err(INVALID.to_owned());
    }
    Ok(Draft {
        kind: PRODUCT_FEEDBACK_KIND,
        content,
        tags,
    })
}

/// The type a file's extension declares, standing in for the browser's
/// `File.type`.
fn declared_image_type(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jfif" | "pjpeg" | "pjp" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// Only supported still images are uploaded (`feedbackImage`): the bytes
/// must be a PNG, JPEG, GIF or WebP matching the file's extension, at most
/// 50 MiB (GIFs 10 MiB). The upload path then strips their metadata.
pub fn image(name: &str, bytes: Vec<u8>) -> Result<UploadFile, String> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(IMAGE_TOO_LARGE.to_owned());
    }
    let kind = sniff_image(&bytes);
    match kind {
        Some(kind)
            if Some(kind) == declared_image_type(name)
                && !(kind == "image/gif" && bytes.len() > MAX_GIF_BYTES) =>
        {
            Ok(UploadFile {
                name: name.to_owned(),
                mime: kind.to_owned(),
                bytes,
            })
        }
        _ => Err(NOT_AN_IMAGE.to_owned()),
    }
}

/// Stands in for the diagnostics file while checking the event size
/// before it is uploaded. Upload results are confined to this origin, a
/// 64-hex hash and at most eight extension characters, so this reserves
/// the longest URL and type a real result can have.
pub fn diagnostics_placeholder(size: u64, origin: &RelayOrigin) -> Attachment {
    let hash = "0".repeat(64);
    Attachment {
        name: DIAGNOSTICS_NAME.to_owned(),
        url: format!(
            "{}/media/{hash}.abcdefgh",
            origin.http().trim_end_matches('/')
        ),
        mime: "application/octet-stream".to_owned(),
        size,
        sha256: hash,
        voice_duration: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> RelayOrigin {
        RelayOrigin::parse("https://relay.example").unwrap()
    }

    fn sha() -> String {
        "ab".repeat(32)
    }

    fn photo() -> Attachment {
        Attachment {
            name: "shot.png".into(),
            url: format!("https://relay.example/media/{}.png", sha()),
            mime: "image/png".into(),
            size: 1234,
            sha256: sha(),
            voice_duration: None,
        }
    }

    #[test]
    fn builds_text_and_category() {
        let draft = event("  It broke  ", Some(Category::NeedsWork), &[], &origin()).unwrap();
        assert_eq!(draft.kind, 42000);
        assert_eq!(draft.content, "It broke");
        assert_eq!(draft.tags, vec![vec!["category", "needs-work"]]);

        let plain = event("Thanks", None, &[], &origin()).unwrap();
        assert!(plain.tags.is_empty());
    }

    #[test]
    fn attachments_become_links_and_imeta() {
        let draft = event("See this", Some(Category::Bug), &[photo()], &origin()).unwrap();
        assert_eq!(
            draft.content,
            format!(
                "See this\n\n![shot.png](<https://relay.example/media/{}.png>)",
                sha()
            )
        );
        assert_eq!(draft.tags[0], vec!["category", "bug"]);
        assert_eq!(draft.tags[1][0], "imeta");
        assert_eq!(draft.tags[1][2], "m image/png");

        let foreign = Attachment {
            url: format!("https://elsewhere.example/media/{}.png", sha()),
            ..photo()
        };
        assert!(event("x", None, &[foreign], &origin()).is_err());
    }

    #[test]
    fn refuses_blank_or_oversized_feedback() {
        assert_eq!(event("   ", None, &[], &origin()).unwrap_err(), INVALID);
        // Attachments alone are not feedback.
        assert_eq!(event("", None, &[photo()], &origin()).unwrap_err(), INVALID);
        let fits = "a".repeat(MAX_INPUT_BYTES - 200);
        assert!(event(&fits, None, &[], &origin()).is_ok());
        let long = "a".repeat(MAX_INPUT_BYTES);
        assert_eq!(event(&long, None, &[], &origin()).unwrap_err(), INVALID);
        // Escaped characters count as serialized.
        let quotes = "\"".repeat(MAX_INPUT_BYTES / 2);
        assert_eq!(event(&quotes, None, &[], &origin()).unwrap_err(), INVALID);
    }

    #[test]
    fn placeholder_reserves_a_valid_upload() {
        let reserved = diagnostics_placeholder(300, &origin());
        let draft = event("Hello", None, &[reserved], &origin()).unwrap();
        assert!(draft.content.contains("/media/0000"));
        assert!(draft.tags[0].contains(&"m application/octet-stream".to_owned()));
        assert!(draft.tags[0].contains(&format!("filename {DIAGNOSTICS_NAME}")));
    }

    #[test]
    fn images_must_match_their_extension() {
        let png = b"\x89PNG\r\n\x1a\n0000".to_vec();
        let file = image("a.PNG", png.clone()).unwrap();
        assert_eq!(file.mime, "image/png");
        assert_eq!(image("a.jpg", png.clone()).unwrap_err(), NOT_AN_IMAGE);
        assert_eq!(image("a.txt", b"hello".to_vec()).unwrap_err(), NOT_AN_IMAGE);
        assert_eq!(image("a.png", Vec::new()).unwrap_err(), IMAGE_TOO_LARGE);
        let jpeg = [0xff, 0xd8, 0xff, 0xe0].to_vec();
        assert_eq!(image("a.jpeg", jpeg).unwrap().mime, "image/jpeg");
        let mut gif = b"GIF89a".to_vec();
        gif.resize(MAX_GIF_BYTES + 1, 0);
        assert_eq!(image("a.gif", gif).unwrap_err(), NOT_AN_IMAGE);
        let mut big = png;
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(image("a.png", big).unwrap_err(), IMAGE_TOO_LARGE);
    }
}
