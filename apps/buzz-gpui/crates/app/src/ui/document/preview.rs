//! One-line summaries of conversation messages (`preview.ts`): text and
//! vetted images, never rendered HTML.

use super::attachments::{self, MediaKind, Metadata};
use super::html::{self, Node as HtmlNode};
use markdown::mdast::Node;
use std::collections::HashMap;

pub const LONG: &str = "Long message · expand to read";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub text: String,
    pub images: Vec<String>,
}

/// The body's CommonMark tree (`scanMarkdown`), or `None` past the size
/// and depth limits.
fn scan(body: &str) -> Option<Node> {
    if body.chars().count() > attachments::MAX_MARKDOWN_LENGTH {
        return None;
    }
    // The bounded GFM read guards the depth before the plain read.
    buzz_relay_client::fold::message_markdown(body)?;
    markdown::to_mdast(body, &markdown::ParseOptions::default()).ok()
}

fn definitions(tree: &Node) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            out.entry(definition.identifier.to_lowercase())
                .or_insert_with(|| definition.url.clone());
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    out
}

/// Links and link references in document order.
fn links(tree: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if matches!(node, Node::Link(_) | Node::LinkReference(_)) {
            out.push(node);
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    out
}

fn destination<'a>(node: &'a Node, definitions: &'a HashMap<String, String>) -> &'a str {
    match node {
        Node::Link(link) => &link.url,
        Node::Image(image) => &image.url,
        Node::LinkReference(link) => definitions
            .get(&link.identifier.to_lowercase())
            .map_or("", String::as_str),
        Node::ImageReference(image) => definitions
            .get(&image.identifier.to_lowercase())
            .map_or("", String::as_str),
        _ => "",
    }
}

struct Images<'a> {
    base: &'a str,
    metadata: &'a Metadata,
    found: Vec<String>,
}

impl Images<'_> {
    fn add(&mut self, source: &str) {
        if let Some(url) = attachments::body_url(source, self.base)
            && attachments::attachment_for(&url, self.metadata, true).is_some()
            && !self.found.contains(&url)
        {
            self.found.push(url);
        }
    }
}

const BLOCKS: &[&str] = &[
    "p", "div", "li", "summary", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// Raw HTML's readable text, with its images collected.
fn html_text(nodes: &[HtmlNode], images: &mut Images, out: &mut String) {
    for node in nodes {
        match node {
            HtmlNode::Text(text) => out.push_str(text),
            HtmlNode::Raw(_) => {}
            HtmlNode::Element(element) => match element.name.as_str() {
                "script" | "style" | "template" => {}
                "br" => out.push('\n'),
                "img" => images.add(element.attr("src").unwrap_or("")),
                name => {
                    html_text(&element.children, images, out);
                    if BLOCKS.contains(&name) {
                        out.push('\n');
                    }
                }
            },
        }
    }
}

fn text(node: &Node, definitions: &HashMap<String, String>, images: &mut Images) -> String {
    match node {
        Node::Definition(_) => String::new(),
        Node::Image(_) | Node::ImageReference(_) => {
            images.add(destination(node, definitions));
            String::new()
        }
        Node::Html(raw) => {
            let mut out = String::new();
            html_text(&html::parse(&raw.value), images, &mut out);
            out
        }
        Node::Text(t) => t.value.clone(),
        Node::InlineCode(c) => c.value.clone(),
        Node::Code(c) => c.value.clone(),
        other => {
            let separator = if matches!(
                other,
                Node::Root(_) | Node::List(_) | Node::ListItem(_) | Node::Blockquote(_)
            ) {
                "\n"
            } else {
                ""
            };
            other
                .children()
                .map(|children| {
                    children
                        .iter()
                        .map(|child| text(child, definitions, images))
                        .collect::<Vec<_>>()
                        .join(separator)
                })
                .unwrap_or_default()
        }
    }
}

/// The first readable line and the vetted images of a body.
pub fn preview(body: &str, body_html: Option<&str>, base: &str) -> Preview {
    let Some(tree) = scan(body) else {
        return Preview {
            text: LONG.into(),
            images: Vec::new(),
        };
    };
    let metadata = attachments::attachment_metadata(body_html);
    let definitions = definitions(&tree);
    let mut images = Images {
        base,
        metadata: &metadata,
        found: Vec::new(),
    };
    // Every block is walked for attachments; only the first readable
    // line is used.
    let lines: Vec<String> = tree
        .children()
        .map(|children| {
            children
                .iter()
                .map(|child| text(child, &definitions, &mut images))
                .collect()
        })
        .unwrap_or_default();
    for link in links(&tree) {
        if let Some(safe) = attachments::body_url(destination(link, &definitions), base)
            && attachments::attachment_for(&safe, &metadata, false)
                .is_some_and(|a| a.kind == MediaKind::Image)
        {
            images.add(&safe);
        }
    }
    let first = lines
        .iter()
        .flat_map(|line| line.split('\n'))
        .map(|line| line.trim_end_matches('\r').trim())
        .find(|line| !line.is_empty());
    let images = images.found;
    let text = match first {
        Some(first) => first
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(280)
            .collect(),
        None if !images.is_empty() => "Images attached".into(),
        None if !body.trim().is_empty() => "Message · expand to read".into(),
        None => String::new(),
    };
    Preview { text, images }
}

/// Whether the body is text only, so it can be measured for one line
/// without loading hidden media (`isTextBody`).
pub fn is_text_body(body: &str, body_html: Option<&str>, base: &str) -> bool {
    let Some(tree) = scan(body) else {
        return false;
    };
    let metadata = attachments::attachment_metadata(body_html);
    let definitions = definitions(&tree);
    let media = |value: &str| {
        attachments::body_url(value, base)
            .is_some_and(|source| attachments::attachment_for(&source, &metadata, false).is_some())
    };
    let mut pending = vec![&tree];
    while let Some(node) = pending.pop() {
        match node {
            Node::Image(_) | Node::ImageReference(_) => return false,
            Node::Link(_) | Node::LinkReference(_) if media(destination(node, &definitions)) => {
                return false;
            }
            Node::Html(raw) => {
                let nodes = html::parse(&raw.value);
                let mut found = false;
                html::walk(&nodes, &mut |element, _| match element.name.as_str() {
                    "img" => found = true,
                    "a" => found |= media(element.attr("href").unwrap_or("")),
                    "video" | "audio" | "source" => {
                        found |= media(element.attr("src").unwrap_or(""))
                    }
                    _ => {}
                });
                if found {
                    return false;
                }
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter());
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://github.com/sample/project/pull/1";
    const IMAGE: &str = "https://raw.githubusercontent.com/sample/project/main/example.png";

    #[test]
    fn extracts_the_first_readable_line() {
        assert_eq!(
            preview(
                "\n**Keep** the [context](https://example.test) readable.\n\nLater line",
                None,
                BASE
            )
            .text,
            "Keep the context readable."
        );
        assert_eq!(
            preview(
                "<div><p>First line</p><p>Second line</p><script>ignored()</script></div>",
                None,
                BASE
            )
            .text,
            "First line"
        );
        assert_eq!(
            preview("<a href='https://example.test'>Linked</a> text", None, BASE).text,
            "Linked text"
        );
    }

    #[test]
    fn collects_vetted_images_in_order() {
        let body = format!(
            "![One]({IMAGE})\n\n![again][same]\n\n[same]: {IMAGE}\n\n<img src='{IMAGE}?second' onerror='bad()'>\n\n`![Not media]({IMAGE}?code)`\n\n![unsafe](javascript:bad)\n\n![other](https://example.test/other.png)"
        );
        assert_eq!(
            preview(&body, None, BASE).images,
            [IMAGE.to_string(), format!("{IMAGE}?second")]
        );
    }

    #[test]
    fn has_image_only_empty_and_bounded_fallbacks() {
        assert_eq!(
            preview(&format!("![One]({IMAGE})"), None, BASE),
            Preview {
                text: "Images attached".into(),
                images: vec![IMAGE.into()]
            }
        );
        assert_eq!(preview("", None, BASE), Preview::default());
        assert_eq!(
            preview(&"x".repeat(100_001), None, BASE),
            Preview {
                text: LONG.into(),
                images: vec![]
            }
        );
        assert_eq!(
            preview(&format!("{}too deep", "> ".repeat(110)), None, BASE).text,
            LONG
        );
    }

    #[test]
    fn previews_image_links_by_renderer_metadata() {
        let asset =
            "https://github.com/user-attachments/assets/81e77cff-0e61-4694-9a88-add0c1f997e4";
        assert_eq!(
            preview(
                &format!("[Screenshot]({asset})"),
                Some(&format!("<img src=\"{asset}\">")),
                BASE
            )
            .images,
            [asset]
        );
        assert!(
            preview(
                &format!("[Video]({asset})"),
                Some(&format!("<video src=\"{asset}\"></video>")),
                BASE
            )
            .images
            .is_empty()
        );
    }

    #[test]
    fn measures_text_bodies_without_media() {
        for body in [
            "",
            "A **formatted** note",
            "A [linked note](https://example.test)",
            "`Code` and ~~edits~~",
            "One\nTwo",
            "First\n\nSecond",
            "# Heading",
            "- First\n- Second",
            "<div>HTML body</div>",
        ] {
            assert!(is_text_body(body, None, BASE), "{body}");
        }
        for body in [
            format!("![Image]({IMAGE})"),
            format!("[Image]({IMAGE})"),
            format!("<img src=\"{IMAGE}\">"),
            format!("<a href=\"{IMAGE}\">Image</a>"),
        ] {
            assert!(!is_text_body(&body, None, BASE), "{body}");
        }
    }
}
