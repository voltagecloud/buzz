//! Which GitHub body links are media (`attachments.ts`): uploads on
//! GitHub's own hosts, typed by the rendered HTML GitHub sends alongside
//! the Markdown, or by their file name.

use super::html;
use std::collections::HashMap;

/// `MAX_MARKDOWN_LENGTH` in buzz-app's message content.
pub const MAX_MARKDOWN_LENGTH: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    pub kind: MediaKind,
    pub name: Option<String>,
}

pub type Metadata = HashMap<String, Descriptor>;

/// A body link shown as a player or preview.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    /// The link as written in the body.
    pub url: String,
    pub kind: MediaKind,
    pub name: Option<String>,
}

/// A link in a body resolved against the page it came from: web links
/// only, never with credentials.
pub fn body_url(value: &str, base: &str) -> Option<String> {
    let url = url::Url::parse(base).ok()?.join(value).ok()?;
    (matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// The upload id in `/user-attachments/assets/<uuid>`.
fn asset_path(path: &str) -> Option<&str> {
    path.strip_prefix("/user-attachments/assets/")
        .filter(|id| uuid(id))
}

/// The upload id in `/<owner>/<repo>/assets/<n>/<uuid>`.
fn legacy_path(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    match parts.as_slice() {
        [owner, repo, "assets", number, id]
            if !owner.is_empty() && !repo.is_empty() && digits(number) && uuid(id) =>
        {
            Some(id)
        }
        _ => None,
    }
}

/// The upload id in a CDN path, `/<n>/<n>-<uuid>.<ext>`.
fn cdn_path(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let [user, file] = parts.as_slice() else {
        return None;
    };
    let (number, rest) = file.split_once('-')?;
    let (id, extension) = rest.rsplit_once('.')?;
    (digits(user)
        && digits(number)
        && uuid(id)
        && !extension.is_empty()
        && extension.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then_some(id)
}

/// An upload URL on one of GitHub's media hosts.
pub fn github_media_url(value: &str) -> Option<url::Url> {
    let safe = body_url(value, "https://github.com")?;
    let url = url::Url::parse(&safe).ok()?;
    if url.scheme() != "https" || url.port().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path();
    let ok = if host == "github.com" {
        asset_path(path).is_some()
            || legacy_path(path).is_some()
            || path
                .strip_prefix("/user-attachments/files/")
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(id, name)| digits(id) && !name.is_empty() && !name.contains('/'))
    } else {
        matches!(
            host,
            "user-images.githubusercontent.com"
                | "private-user-images.githubusercontent.com"
                | "raw.githubusercontent.com"
        )
    };
    ok.then_some(url)
}

/// Uploads are keyed by their id, so a signed CDN copy in the HTML
/// matches the `github.com` link in the Markdown.
fn asset_key(url: &url::Url) -> String {
    let id = if url.host_str() == Some("github.com") {
        asset_path(url.path()).or_else(|| legacy_path(url.path()))
    } else {
        cdn_path(url.path())
    };
    match id {
        Some(id) => format!("asset:{}", id.to_lowercase()),
        None => url.to_string(),
    }
}

fn filename_kind(name: &str) -> Option<MediaKind> {
    let extension = name.rsplit_once('.')?.1.to_lowercase();
    match extension.as_str() {
        "png" | "gif" | "jpg" | "jpeg" | "svg" | "webp" => Some(MediaKind::Image),
        "mp4" | "mov" | "webm" => Some(MediaKind::Video),
        "mp3" | "wav" | "m4a" | "ogg" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// Media types from the rendered HTML, by upload.
pub fn attachment_metadata(body_html: Option<&str>) -> Metadata {
    let mut descriptors = Metadata::new();
    let Some(body_html) = body_html.filter(|h| !h.is_empty()) else {
        return descriptors;
    };
    if body_html.len() > MAX_MARKDOWN_LENGTH * 10 {
        return descriptors;
    }
    let nodes = html::parse(body_html);
    html::walk(&nodes, &mut |element, ancestors| {
        if !matches!(
            element.name.as_str(),
            "img" | "video" | "audio" | "source" | "a"
        ) {
            return;
        }
        let tag = if element.name == "source" {
            ancestors.last().map_or("", |parent| parent.name.as_str())
        } else {
            element.name.as_str()
        };
        let name = if tag == "a" {
            Some(html::text_content(&element.children))
        } else {
            std::iter::once(element)
                .chain(ancestors.iter().rev().copied())
                .find(|e| e.name == "details")
                .and_then(|details| html::find(&details.children, "summary"))
                .map(|summary| html::text_content(&summary.children))
        }
        .map(|name| name.trim().to_string());
        for attribute in ["src", "href", "data-canonical-src"] {
            let Some(url) = github_media_url(element.attr(attribute).unwrap_or("")) else {
                continue;
            };
            let kind = match tag {
                "img" => Some(MediaKind::Image),
                "video" => Some(MediaKind::Video),
                "audio" => Some(MediaKind::Audio),
                _ => filename_kind(name.as_deref().unwrap_or(url.path())),
            };
            if let Some(kind) = kind {
                descriptors.insert(
                    asset_key(&url),
                    Descriptor {
                        kind,
                        name: name.clone(),
                    },
                );
            }
        }
    });
    descriptors
}

/// The attachment a body link shows, if it is media. `image` is for
/// Markdown and HTML images, which are images whatever their name.
pub fn attachment_for(value: &str, metadata: &Metadata, image: bool) -> Option<Attachment> {
    let url = github_media_url(value)?;
    let descriptor = metadata.get(&asset_key(&url));
    let kind = if image {
        Some(MediaKind::Image)
    } else {
        descriptor
            .map(|d| d.kind)
            .or_else(|| filename_kind(url.path()))
    }?;
    if url.host_str() == Some("raw.githubusercontent.com") && kind != MediaKind::Image {
        return None;
    }
    Some(Attachment {
        url: value.to_string(),
        kind,
        name: descriptor
            .and_then(|d| d.name.clone())
            .filter(|n| !n.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str =
        "https://github.com/user-attachments/assets/81e77cff-0e61-4694-9a88-add0c1f997e4";
    const AFTER: &str =
        "https://github.com/user-attachments/assets/dd1f7e4f-e2a4-4aec-b661-cb45032f29c5";
    const AUDIO: &str = "https://github.com/user-attachments/files/123/recording.wav";
    const METADATA: &str = "<details><summary>before.mp4</summary><video src=\"https://private-user-images.githubusercontent.com/1888043/659873452-81e77cff-0e61-4694-9a88-add0c1f997e4.mp4?jwt=expired\"></video></details><video><source src=\"https://github.com/user-attachments/assets/dd1f7e4f-e2a4-4aec-b661-cb45032f29c5\" /></video>";

    #[test]
    fn types_uploads_from_rendered_html() {
        let metadata = attachment_metadata(Some(METADATA));
        let before = attachment_for(BEFORE, &metadata, false).unwrap();
        assert_eq!(before.kind, MediaKind::Video);
        assert_eq!(before.name.as_deref(), Some("before.mp4"));
        assert_eq!(
            attachment_for(AFTER, &metadata, false).map(|a| a.kind),
            Some(MediaKind::Video)
        );
        assert_eq!(
            attachment_for(AUDIO, &Metadata::new(), false).map(|a| a.kind),
            Some(MediaKind::Audio)
        );
        // Named HTML anchors type their upload.
        let named = attachment_metadata(Some(&format!("<a href=\"{AFTER}\">capture.mov</a>")));
        let movie = attachment_for(AFTER, &named, false).unwrap();
        assert_eq!(movie.kind, MediaKind::Video);
        assert_eq!(movie.name.as_deref(), Some("capture.mov"));
    }

    #[test]
    fn does_not_guess_uuid_media_or_match_embedded_ids() {
        assert_eq!(attachment_for(BEFORE, &Metadata::new(), false), None);
        let embedded = attachment_metadata(Some(
            "<video src=\"https://private-user-images.githubusercontent.com/1888043/prefix-81e77cff-0e61-4694-9a88-add0c1f997e4.mp4\"></video>",
        ));
        assert_eq!(attachment_for(BEFORE, &embedded, false), None);
    }

    #[test]
    fn keeps_media_to_github_hosts() {
        for url in [
            "https://github.com.evil.test/image.png",
            "https://example.com/image.png",
            "http://github.com/user-attachments/files/1/a.png",
            "https://github.com:8443/user-attachments/files/1/a.png",
        ] {
            assert_eq!(attachment_for(url, &Metadata::new(), true), None, "{url}");
        }
        let raw = "https://raw.githubusercontent.com/sample/project/main/clip.mp4";
        assert_eq!(attachment_for(raw, &Metadata::new(), false), None);
        assert!(attachment_for(raw, &Metadata::new(), true).is_some());
        assert_eq!(
            attachment_for(
                "https://github.com/user-attachments/files/99/source.zip",
                &Metadata::new(),
                false
            ),
            None
        );
    }

    #[test]
    fn resolves_body_links_safely() {
        let base = "https://github.com/block/buzz-app/pull/327";
        assert_eq!(
            body_url("../328", base).as_deref(),
            Some("https://github.com/block/buzz-app/328")
        );
        assert_eq!(body_url("javascript:alert(1)", base), None);
        assert_eq!(body_url("data:text/html,bad", base), None);
        assert_eq!(
            body_url("https://user:password@example.com/file", base),
            None
        );
    }
}
