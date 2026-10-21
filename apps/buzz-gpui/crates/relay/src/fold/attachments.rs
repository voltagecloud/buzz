//! Reading attachments: `imeta` tags plus Markdown images and attachment
//! links in the body (`fold.ts:25-165`, `message-content.ts`, §5n.4).
//! Signed metadata is still untrusted layout input, so every field is
//! validated and bounded here.

use markdown::mdast::Node;
use nostr::prelude::Event;
use std::collections::{HashMap, HashSet};
use url::Url;

/// Longest duration we show (`contracts.ts:6`).
const MAX_DURATION_SECONDS: f64 = 86_400.0;
/// Longest attachment name, in UTF-16 units as in the reference.
const MAX_NAME_LENGTH: usize = 256;
/// Deeper Markdown is shown as-is rather than projected.
const MAX_MARKDOWN_DEPTH: usize = 100;
/// The parser recurses and slows down on deep nesting, so hostile content
/// could abort the process with a stack overflow. Content whose
/// [`depth_bound`] exceeds this is not parsed at all.
const PARSE_DEPTH_BUDGET: usize = 1_000;
/// JS `Number.MAX_SAFE_INTEGER`; larger claimed sizes are dropped.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// HTTPS only, credential-free, WHATWG-serialized.
    pub url: String,
    pub kind: AttachmentKind,
    /// Sender claims: `size` is not checked, and `name` is for display and
    /// download only.
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub name: Option<String>,
    /// Seconds; a display hint the player corrects.
    pub duration: Option<f64>,
    pub waveform: Option<Vec<u8>>,
    pub voice_note: bool,
    pub dimensions: Option<Dimensions>,
    pub blurhash: Option<String>,
    /// Signed video poster or thumbnail URL.
    pub preview_url: Option<String>,
    /// Served over plain HTTP. Media stays HTTPS-only, as in the reference
    /// (`safeMessageUrl`), but where the reference drops it this shows an
    /// unavailable file card, so the attachment does not vanish.
    pub insecure: bool,
}

impl Attachment {
    fn bare(url: String, kind: AttachmentKind) -> Self {
        Self {
            url,
            kind,
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
        }
    }

    /// A plain-HTTP URL as an unavailable file.
    fn insecure(url: String, name: Option<String>) -> Self {
        Self {
            name,
            insecure: true,
            ..Self::bare(url, AttachmentKind::File)
        }
    }
}

/// Attachments from the event's `imeta` tags, then from Markdown image URLs
/// in the body that no `imeta` covered. `link_names` are labels of
/// attachment links in the body, which take precedence over `filename`.
pub fn parse_attachments(
    event: &Event,
    markdown_images: &[String],
    link_names: &HashMap<String, String>,
) -> Vec<Attachment> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for tag in event.tags.iter() {
        let tag = tag.as_slice();
        if tag.first().map(String::as_str) != Some("imeta") {
            continue;
        }
        let fields = imeta_fields(tag);
        let Some(url) = fields.get("url").and_then(|url| safe_link_url(url)) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let field = |name: &str| fields.get(name).copied().filter(|v| !v.is_empty());
        let filename = field("filename").and_then(safe_attachment_name);
        let basename = url_basename(&url);
        let link_name = link_names.get(&url).cloned();
        if !url.starts_with("https:") {
            result.push(Attachment {
                mime: field("m").map(str::to_owned),
                size: fields.get("size").copied().and_then(parse_size),
                ..Attachment::insecure(url, link_name.or(filename).or(basename))
            });
            continue;
        }
        let detection = filename
            .clone()
            .or_else(|| link_name.clone())
            .or_else(|| basename.clone())
            .map(|name| name.to_lowercase());
        let mime = field("m");
        let kind = attachment_kind(mime, &url, detection.as_deref());
        let preview_url = if field("image").or(field("thumb")).is_some() {
            // `image ?? thumb`: an empty `image` still shadows `thumb`.
            safe_message_url(
                fields
                    .get("image")
                    .or(fields.get("thumb"))
                    .copied()
                    .unwrap_or(""),
            )
        } else {
            None
        };
        result.push(Attachment {
            mime: mime.map(str::to_owned),
            size: fields.get("size").copied().and_then(parse_size),
            name: link_name.or(filename).or(basename),
            duration: fields.get("duration").copied().and_then(parse_duration),
            waveform: fields.get("waveform").copied().and_then(parse_waveform),
            voice_note: kind == AttachmentKind::Audio
                && detection.is_some_and(|name| name.starts_with("voice-note-")),
            dimensions: fields.get("dim").copied().and_then(parse_dimensions),
            blurhash: fields.get("blurhash").copied().and_then(validated_blurhash),
            preview_url,
            ..Attachment::bare(url, kind)
        });
    }
    for url in markdown_images {
        if !seen.insert(url.clone()) {
            continue;
        }
        if !url.starts_with("https:") {
            let name = url_basename(url);
            result.push(Attachment::insecure(url.clone(), name));
        } else {
            let kind = if video_extension(url) {
                AttachmentKind::Video
            } else {
                AttachmentKind::Image
            };
            result.push(Attachment::bare(url.clone(), kind));
        }
    }
    result
}

/// The link-safe URLs named by this event's `imeta` tags, plain HTTP
/// included: their body links give way to an unavailable file card.
pub(super) fn imeta_urls(event: &Event) -> HashSet<String> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
        .filter_map(|tag| {
            imeta_fields(tag)
                .get("url")
                .and_then(|url| safe_link_url(url))
        })
        .collect()
}

/// `"key value"` fields; a repeated key keeps its last value, as
/// `Object.fromEntries` does.
fn imeta_fields(tag: &[String]) -> HashMap<&str, &str> {
    tag.iter()
        .skip(1)
        .filter_map(|field| field.split_once(' '))
        .collect()
}

fn attachment_kind(mime: Option<&str>, url: &str, detection: Option<&str>) -> AttachmentKind {
    let lower = mime.map(str::to_lowercase);
    let lower = lower.as_deref();
    if lower.is_some_and(|m| m.starts_with("image/")) {
        AttachmentKind::Image
    } else if lower.is_some_and(|m| m.starts_with("audio/")) || legacy_voice_note(lower, detection)
    {
        AttachmentKind::Audio
    } else if lower.is_some_and(|m| m.starts_with("video/")) {
        AttachmentKind::Video
    } else if mime.is_some() {
        AttachmentKind::File
    } else if video_extension(url) {
        AttachmentKind::Video
    } else if has_extension(url, &["png", "jpg", "jpeg", "gif", "webp", "avif"]) {
        AttachmentKind::Image
    } else {
        AttachmentKind::File
    }
}

/// Old Buzz uploaded voice notes as `video/mp4` (a 16x16 black H.264 track
/// plus AAC) because the relay's video validator needs a video track. Only
/// the filename convention tells them apart.
fn legacy_voice_note(mime: Option<&str>, name: Option<&str>) -> bool {
    mime.is_some_and(|m| m == "video/mp4" || m.starts_with("video/mp4;"))
        && name.is_some_and(|n| n.starts_with("voice-note-") && n.ends_with(".mp4"))
}

fn video_extension(url: &str) -> bool {
    has_extension(url, &["mp4", "webm"])
}

/// `/\.(ext)(?:\?|$)/i` anywhere in the URL.
fn has_extension(url: &str, extensions: &[&str]) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.match_indices('.').any(|(dot, _)| {
        let rest = &lower[dot + 1..];
        extensions.iter().any(|ext| {
            rest.strip_prefix(ext)
                .is_some_and(|after| after.is_empty() || after.starts_with('?'))
        })
    })
}

fn parse_size(value: &str) -> Option<u64> {
    let digits = value.as_bytes();
    let canonical = digits.first().is_some_and(|b| matches!(b, b'1'..=b'9'))
        && digits.iter().all(u8::is_ascii_digit);
    canonical
        .then(|| value.parse::<u64>().ok())
        .flatten()
        .filter(|size| *size <= MAX_SAFE_INTEGER)
}

/// `^\d+(?:\.\d+)?$`, then `0 < d <= 86400`.
fn parse_duration(value: &str) -> Option<f64> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let decimal = match value.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(value),
    };
    if !decimal {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|d| *d > 0.0 && *d <= MAX_DURATION_SECONDS)
}

/// 1 to 100 integers in 0..=100, separated by whitespace.
fn parse_waveform(value: &str) -> Option<Vec<u8>> {
    let samples: Option<Vec<u8>> = value
        .split_whitespace()
        .map(|n| n.parse::<u8>().ok().filter(|n| *n <= 100))
        .collect();
    samples.filter(|s| (1..=100).contains(&s.len()))
}

/// `^(\d{1,6})x(\d{1,6})$` with both sides positive.
fn parse_dimensions(value: &str) -> Option<Dimensions> {
    let (width, height) = value.split_once('x')?;
    let side = |s: &str| {
        ((1..=6).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
            .filter(|n| *n > 0)
    };
    Some(Dimensions {
        width: side(width)?,
        height: side(height)?,
    })
}

const BASE83: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

/// Syntax check only, so folding never decodes pixels (`blurhash.ts`): at
/// most 9x9 components, and a length that matches the size flag.
fn validated_blurhash(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if !(6..=166).contains(&bytes.len()) || !bytes.iter().all(|b| BASE83.contains(b)) {
        return None;
    }
    let size = BASE83.iter().position(|b| *b == bytes[0])?;
    if size > 80 {
        return None;
    }
    let components = (size % 9 + 1) * (size / 9 + 1);
    (bytes.len() == 4 + 2 * components).then(|| value.to_owned())
}

/// Raw HTML in chat is shown as the typed text, except complete HTML
/// comments (`visibleHtmlSource`). Comments are hidden markup by Markdown
/// convention, and clients put protocol markers in them (Buzz Desktop
/// waves start with `<!-- buzz:wave:v1 -->`). An unclosed `<!--` stays
/// visible.
pub fn visible_html_source(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("<!--") {
        let Some(end) = rest[start + 4..].find("-->") else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + 4 + end + 3..];
    }
    out.push_str(rest);
    out
}

/// Credential-free HTTP(S), WHATWG-serialized (`message-content.ts:6-18`).
pub(super) fn safe_link_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.into())
}

/// Attachments stay HTTPS-only.
pub(super) fn safe_message_url(value: &str) -> Option<String> {
    safe_link_url(value).filter(|url| url.starts_with("https:"))
}

/// The last path segment as a display name, unless it is the relay's
/// content-hash name.
fn url_basename(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let segment = url.path().rsplit('/').next().filter(|s| !s.is_empty())?;
    let decoded = decode_uri_component(segment).unwrap_or_else(|| segment.to_owned());
    if relay_hash_basename(&decoded) {
        None
    } else {
        safe_attachment_name(&decoded)
    }
}

/// JS `decodeURIComponent`: `None` on a malformed escape or invalid UTF-8.
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |b: u8| (b as char).to_digit(16);
            let high = hex(*bytes.get(i + 1)?)?;
            let low = hex(*bytes.get(i + 2)?)?;
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `^[0-9a-f]{64}(?:\.[^./?#]+)?$`, case-insensitive.
fn relay_hash_basename(value: &str) -> bool {
    let (hash, extension) = match value.get(..64) {
        Some(hash) => (hash, &value[64..]),
        None => return false,
    };
    hash.bytes().all(|b| b.is_ascii_hexdigit())
        && (extension.is_empty()
            || extension
                .strip_prefix('.')
                .is_some_and(|ext| !ext.is_empty() && !ext.contains(['.', '/', '?', '#'])))
}

/// Bounded, and free of control and bidi-override characters that could
/// disguise a file type (`message-content.ts:50-65`).
pub(super) fn safe_attachment_name(value: &str) -> Option<String> {
    let mut units = 0;
    let end = value
        .char_indices()
        .find(|(_, c)| {
            units += c.len_utf16();
            units > MAX_NAME_LENGTH
        })
        .map_or(value.len(), |(index, _)| index);
    let name = &value[..end];
    let unsafe_char = |c: char| {
        matches!(c as u32,
            0..=0x1f | 0x7f..=0x9f | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
    };
    (!name.is_empty() && !name.chars().any(unsafe_char)).then(|| name.to_owned())
}

/// A body with attachment Markdown removed (`projectMarkdownAttachments`).
pub(super) struct Projected {
    pub content: String,
    pub seams: Vec<usize>,
    /// Link-safe image URLs, in document order. Plain-HTTP ones become
    /// unavailable file cards.
    pub urls: Vec<String>,
    /// `(url, label)` of attachment links, in document order.
    pub names: Vec<(String, String)>,
}

impl Projected {
    pub fn unchanged(content: &str) -> Self {
        Self {
            content: content.to_owned(),
            seams: Vec::new(),
            urls: Vec::new(),
            names: Vec::new(),
        }
    }
}

/// Strips every image and every link to an `imeta` URL from the body, and
/// collects image URLs and usable link labels. Markdown nested deeper than
/// [`MAX_MARKDOWN_DEPTH`] is left alone.
pub(super) fn project_markdown(content: &str, attachment_urls: &HashSet<String>) -> Projected {
    if depth_bound(content) > PARSE_DEPTH_BUDGET {
        return Projected::unchanged(content);
    }
    let Ok(tree) = markdown::to_mdast(content, &markdown::ParseOptions::default()) else {
        return Projected::unchanged(content);
    };
    let Some(scan) = Scan::new(&tree) else {
        return Projected::unchanged(content);
    };
    let mut urls: Vec<String> = Vec::new();
    let mut names = Vec::new();
    let mut ranges = Vec::new();
    for image in &scan.images {
        if let Some(url) = scan.url(image).and_then(safe_link_url)
            && !urls.contains(&url)
        {
            urls.push(url);
        }
        ranges.extend(span(image));
    }
    if !attachment_urls.is_empty() {
        for link in &scan.links {
            let Some(url) = scan.url(link).and_then(safe_link_url) else {
                continue;
            };
            if !attachment_urls.contains(&url) {
                continue;
            }
            ranges.extend(span(link));
            let label = link.to_string();
            if let Some(label) = safe_attachment_name(label.trim())
                && adoptable_label(&label, &url)
            {
                names.push((url, label));
            }
        }
    }
    let (content, seams) = strip_ranges(content, ranges);
    Projected {
        content,
        seams,
        urls,
        names,
    }
}

/// Longest body rendered as Markdown (`MAX_MARKDOWN_LENGTH`, counted in
/// UTF-16 units there and in characters here).
const MAX_MARKDOWN_LENGTH: usize = 100_000;

/// The GFM tree a message body renders from (`prepareMarkdown`), or `None`
/// when the body must render as plain text: longer than
/// [`MAX_MARKDOWN_LENGTH`] or nested deeper than [`MAX_MARKDOWN_DEPTH`].
pub fn message_markdown(content: &str) -> Option<Node> {
    if content.chars().count() > MAX_MARKDOWN_LENGTH || depth_bound(content) > PARSE_DEPTH_BUDGET {
        return None;
    }
    let tree = markdown::to_mdast(content, &markdown::ParseOptions::gfm()).ok()?;
    let mut pending = vec![(&tree, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_MARKDOWN_DEPTH {
            return None;
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().map(|child| (child, depth + 1)));
        }
    }
    Some(tree)
}

/// An upper bound on the mdast depth, from a linear scan.
///
/// A leaf block sits inside containers that its first line opens or
/// continues, and each level costs that line at least one prefix character
/// (`>`) or two (a list marker or its indent) for two nodes (list and
/// item). Inline nesting inside one paragraph costs at least one `*`, `_`
/// or `[` per level, and a paragraph never spans a blank line. Above the
/// reference's depth limit the result is unprojected either way; the bound
/// is loose only for content with hundreds of such markers.
fn depth_bound(content: &str) -> usize {
    let mut prefix = 0;
    let mut paragraph = 0;
    let mut inline = 0;
    // CommonMark ends lines at `\n`, `\r\n` and a lone `\r`.
    let lines = content
        .split('\n')
        .flat_map(|line| line.strip_suffix('\r').unwrap_or(line).split('\r'));
    for line in lines {
        if line.bytes().all(|b| b == b' ' || b == b'\t') {
            paragraph = 0;
            continue;
        }
        let width: usize = line
            .chars()
            .take_while(|c| {
                matches!(
                    c,
                    ' ' | '\t' | '>' | '-' | '+' | '*' | '.' | ')' | '0'..='9'
                )
            })
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum();
        prefix = prefix.max(width);
        paragraph += line
            .bytes()
            .filter(|b| matches!(b, b'*' | b'_' | b'['))
            .count();
        inline = inline.max(paragraph);
    }
    2 + 2 * prefix + inline
}

struct Scan<'a> {
    definitions: HashMap<String, &'a str>,
    images: Vec<&'a Node>,
    links: Vec<&'a Node>,
}

impl<'a> Scan<'a> {
    /// Pre-order walk with an explicit stack, so hostile nesting cannot
    /// exhaust ours. `None` when the tree is too deep.
    fn new(tree: &'a Node) -> Option<Self> {
        let mut scan = Self {
            definitions: HashMap::new(),
            images: Vec::new(),
            links: Vec::new(),
        };
        let mut pending = vec![(tree, 0)];
        while let Some((node, depth)) = pending.pop() {
            if depth > MAX_MARKDOWN_DEPTH {
                return None;
            }
            match node {
                Node::Definition(definition) => {
                    scan.definitions
                        .entry(definition.identifier.to_lowercase())
                        .or_insert(&definition.url);
                }
                Node::Image(_) | Node::ImageReference(_) => scan.images.push(node),
                Node::Link(_) | Node::LinkReference(_) => scan.links.push(node),
                _ => {}
            }
            if let Some(children) = node.children() {
                pending.extend(children.iter().rev().map(|child| (child, depth + 1)));
            }
        }
        Some(scan)
    }

    fn url(&self, node: &'a Node) -> Option<&'a str> {
        let identifier = match node {
            Node::Image(image) => return Some(&image.url),
            Node::Link(link) => return Some(&link.url),
            Node::ImageReference(image) => &image.identifier,
            Node::LinkReference(link) => &link.identifier,
            _ => return None,
        };
        self.definitions.get(&identifier.to_lowercase()).copied()
    }
}

fn span(node: &Node) -> Option<(usize, usize)> {
    node.position()
        .map(|position| (position.start.offset, position.end.offset))
}

/// A link label can name the attachment unless it is a hash name, a URL,
/// or the link's own URL.
fn adoptable_label(label: &str, url: &str) -> bool {
    !relay_hash_basename(label) && Url::parse(label).is_err() && label != url
}

/// Removes the ranges and records seams: offsets in the result where removed
/// text once separated its neighbors. Only interior seams matter.
fn strip_ranges(content: &str, mut ranges: Vec<(usize, usize)>) -> (String, Vec<usize>) {
    ranges.sort_by_key(|range| range.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(previous) if start <= previous.1 => previous.1 = previous.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut stripped = String::with_capacity(content.len());
    let mut seams = Vec::new();
    let mut offset = 0;
    for (start, end) in merged {
        stripped.push_str(&content[offset..start]);
        seams.push(stripped.len());
        offset = end;
    }
    stripped.push_str(&content[offset..]);
    stripped.truncate(stripped.trim_end().len());
    seams.retain(|seam| *seam > 0 && *seam < stripped.len());
    (stripped, seams)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_typed_html_but_hides_complete_comments() {
        assert_eq!(visible_html_source("<harness>"), "<harness>");
        assert_eq!(
            visible_html_source("<!-- buzz:wave:v1 -->\n<b>hi</b><!--x-->!"),
            "\n<b>hi</b>!"
        );
        assert_eq!(visible_html_source("<!-->"), "<!-->");
        assert_eq!(visible_html_source("a <!-- open"), "a <!-- open");
    }

    #[test]
    fn validates_blurhash_syntax() {
        let max = format!("|000{}", "00".repeat(81));
        for (hash, valid) in [
            ("LEHV6nWB2yk8pyo0adR*.7kCMdnj", true),
            ("000000", true),
            (max.as_str(), true),
            ("short", false),
            ("LEHV6nWB2yk8pyo0adR*.7kCMdn!", false),
            ("LEHV6nWB2yk8pyo0adR*.7kCMdn", false),
            (&format!("~000{}", "00".repeat(18)), false),
            (&"0".repeat(10_000), false),
        ] {
            assert_eq!(validated_blurhash(hash).is_some(), valid, "{hash}");
        }
    }

    #[test]
    fn validates_dimensions() {
        let dims = |w, h| {
            Some(Dimensions {
                width: w,
                height: h,
            })
        };
        assert_eq!(parse_dimensions("700x900"), dims(700, 900));
        assert_eq!(parse_dimensions("1x999999"), dims(1, 999_999));
        for bad in ["0x900", "700x0", "1x2px", "1000000x2", "1x2x3", ""] {
            assert_eq!(parse_dimensions(bad), None, "{bad}");
        }
    }

    #[test]
    fn validates_duration_and_size() {
        assert_eq!(parse_duration("86400"), Some(86_400.0));
        assert_eq!(parse_duration("12.3"), Some(12.3));
        for bad in ["0", " 12 ", "0x10", "86400.1", "1.", ".5"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(parse_size("1536"), Some(1536));
        for bad in ["0", "1.5", "01", &"9".repeat(20), "9007199254740992"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn decodes_uri_components_strictly() {
        assert_eq!(
            decode_uri_component("r%C3%A9sum%C3%A9").as_deref(),
            Some("résumé")
        );
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%E2%80"), None);
    }

    #[test]
    fn survives_hostile_nesting() {
        let image = "![i](https://x.test/i.png)";
        let urls = HashSet::from(["https://x.test/a.pdf".to_owned()]);
        let hostile = [
            ">".repeat(100_000),
            "a\r".to_owned() + &">".repeat(100_000),
            "- ".repeat(50_000),
            "*".repeat(50_000) + "a" + &"*".repeat(50_000),
            "a\n".to_owned() + &"*a ".repeat(30_000),
            "[".repeat(50_000) + "a" + &"](https://x.test/a.pdf)".repeat(50_000),
        ];
        for body in hostile {
            let body = body + image;
            assert_eq!(project_markdown(&body, &urls).content, body);
            assert!(message_markdown(&body).is_none());
        }
        // Under the parse budget, deep but bounded nesting parses safely and
        // the reference's depth limit still applies.
        for body in [
            "- ".repeat(240),
            "> ".repeat(240),
            "a ".to_owned() + &"*".repeat(480) + "a" + &"*".repeat(480),
        ] {
            let body = body + image;
            assert!(depth_bound(&body) <= PARSE_DEPTH_BUDGET);
            assert_eq!(project_markdown(&body, &urls).content, body);
        }
        let shallow = "> - *quoted* ".to_owned() + image;
        assert_eq!(project_markdown(&shallow, &urls).content, "> - *quoted*");
    }

    #[test]
    fn renders_ordinary_bodies_as_gfm() {
        let tree = message_markdown("| a | b |\n|---|---|\n| 1 | ~~2~~ |").unwrap();
        assert!(matches!(tree.children().unwrap()[0], Node::Table(_)));
        assert!(message_markdown(&"a".repeat(MAX_MARKDOWN_LENGTH + 1)).is_none());
    }
    #[test]
    fn matches_extensions_before_a_query_or_the_end() {
        assert!(has_extension("https://x.test/a.MP4?x=1", &["mp4"]));
        assert!(!has_extension("https://x.test/a.mp4x", &["mp4"]));
        assert!(has_extension("https://x.test/a.jpeg", &["jpg", "jpeg"]));
    }
}
