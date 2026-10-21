//! A GitHub body's content (`GitHubBody.tsx`): GitHub-flavoured Markdown
//! with single newlines as breaks, raw HTML reduced to its text, images,
//! links and media (`githubHtml`), and every link resolved against the
//! object's page and kept to the web (`urlTransform`).

use super::attachments::{self, Attachment, Metadata};
use super::html::{self, Node as HtmlNode};
use markdown::mdast::{AlignKind, Node};
use std::collections::HashMap;

/// `MAX_MARKDOWN_DEPTH`: deeper HTML keeps its markup as text.
const MAX_HTML_DEPTH: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    /// `\n` is a line break.
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Delete(Vec<Inline>),
    Code(String),
    /// `url` is `None` when the destination is not a safe web link; the
    /// label still shows.
    Link {
        url: Option<String>,
        children: Vec<Inline>,
    },
    Image {
        url: Option<String>,
        alt: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading(Vec<Inline>),
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u32,
        /// Loose lists keep paragraph spacing inside items.
        spread: bool,
        items: Vec<Item>,
    },
    Code(String),
    Table {
        align: Vec<AlignKind>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    Footnotes(Vec<Vec<Block>>),
}

/// A prepared body.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// Over the size or depth limits: shown as written.
    Plain(String),
    Rich {
        blocks: Vec<Block>,
        metadata: Metadata,
    },
}

/// Prepares `body`, read with its rendered `body_html` and linked from
/// the page at `base`.
pub fn prepare(body: &str, body_html: Option<&str>, base: &str) -> Body {
    let Some(tree) = buzz_relay_client::fold::message_markdown(body) else {
        return Body::Plain(body.to_string());
    };
    let mut definitions = HashMap::new();
    collect_definitions(&tree, &mut definitions);
    let prep = Prep {
        base,
        definitions: &definitions,
    };
    let blocks = match &tree {
        Node::Root(root) => prep.blocks(&root.children),
        _ => Vec::new(),
    };
    Body::Rich {
        blocks,
        metadata: attachments::attachment_metadata(body_html),
    }
}

fn collect_definitions(node: &Node, out: &mut HashMap<String, String>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            out.entry(definition.identifier.to_lowercase())
                .or_insert_with(|| definition.url.clone());
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter());
        }
    }
}

struct Prep<'a> {
    base: &'a str,
    definitions: &'a HashMap<String, String>,
}

impl Prep<'_> {
    fn url(&self, value: &str) -> Option<String> {
        attachments::body_url(value, self.base)
    }

    fn reference(&self, identifier: &str) -> Option<&str> {
        self.definitions
            .get(&identifier.to_lowercase())
            .map(String::as_str)
    }

    /// Children of a block container (root, quote, list item): raw HTML
    /// becomes a paragraph of its content.
    fn blocks(&self, children: &[Node]) -> Vec<Block> {
        children
            .iter()
            .filter_map(|child| match child {
                Node::Html(raw) => Some(Block::Paragraph(self.html(&raw.value))),
                other => self.block(other),
            })
            .collect()
    }

    fn block(&self, node: &Node) -> Option<Block> {
        Some(match node {
            Node::Paragraph(p) => Block::Paragraph(self.inlines(&p.children)),
            Node::Heading(h) => Block::Heading(self.inlines(&h.children)),
            Node::Blockquote(q) => Block::Quote(self.blocks(&q.children)),
            Node::List(list) => Block::List {
                ordered: list.ordered,
                start: list.start.unwrap_or(1),
                spread: list.spread
                    || list
                        .children
                        .iter()
                        .any(|item| matches!(item, Node::ListItem(i) if i.spread)),
                items: list
                    .children
                    .iter()
                    .filter_map(|item| match item {
                        Node::ListItem(item) => Some(Item {
                            checked: item.checked,
                            blocks: self.blocks(&item.children),
                        }),
                        _ => None,
                    })
                    .collect(),
            },
            Node::Code(code) => Block::Code(code.value.clone()),
            Node::Table(table) => Block::Table {
                align: table.align.clone(),
                rows: table
                    .children
                    .iter()
                    .filter_map(|row| match row {
                        Node::TableRow(row) => Some(
                            row.children
                                .iter()
                                .map(|cell| match cell {
                                    Node::TableCell(cell) => self.inlines(&cell.children),
                                    _ => Vec::new(),
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect(),
            },
            Node::ThematicBreak(_) => Block::Rule,
            Node::FootnoteDefinition(note) => Block::Footnotes(vec![self.blocks(&note.children)]),
            _ => return None,
        })
    }

    /// Phrasing children. An inline anchor arrives as opening tag, label
    /// and closing tag siblings; the pair is rejoined so the label stays
    /// linked.
    fn inlines(&self, children: &[Node]) -> Vec<Inline> {
        let mut out = Vec::new();
        let mut index = 0;
        while index < children.len() {
            let node = &children[index];
            let Node::Html(raw) = node else {
                out.extend(self.inline(node));
                index += 1;
                continue;
            };
            let href = anchor_href(&raw.value);
            let closing = href.as_ref().and_then(|_| {
                children[index + 1..]
                    .iter()
                    .position(
                        |sibling| matches!(sibling, Node::Html(h) if closing_anchor(&h.value)),
                    )
                    .map(|at| index + 1 + at)
            });
            match (href, closing) {
                (Some(href), Some(closing)) => {
                    out.push(Inline::Link {
                        url: self.url(&href),
                        children: self.inlines(&children[index + 1..closing]),
                    });
                    index = closing + 1;
                }
                _ => {
                    out.extend(self.html(&raw.value));
                    index += 1;
                }
            }
        }
        out
    }

    fn inline(&self, node: &Node) -> Vec<Inline> {
        vec![match node {
            Node::Text(text) => Inline::Text(text.value.clone()),
            Node::Break(_) => Inline::Text("\n".into()),
            Node::InlineCode(code) => Inline::Code(code.value.clone()),
            Node::Strong(n) => Inline::Strong(self.inlines(&n.children)),
            Node::Emphasis(n) => Inline::Emphasis(self.inlines(&n.children)),
            Node::Delete(n) => Inline::Delete(self.inlines(&n.children)),
            Node::Link(link) => Inline::Link {
                url: self.url(&link.url),
                children: self.inlines(&link.children),
            },
            Node::LinkReference(link) => Inline::Link {
                url: self.reference(&link.identifier).and_then(|u| self.url(u)),
                children: self.inlines(&link.children),
            },
            Node::Image(image) => Inline::Image {
                url: self.url(&image.url),
                alt: image.alt.clone(),
            },
            Node::ImageReference(image) => Inline::Image {
                url: self.reference(&image.identifier).and_then(|u| self.url(u)),
                alt: image.alt.clone(),
            },
            Node::FootnoteReference(note) => Inline::Text(format!(
                "[{}]",
                note.label.as_deref().unwrap_or(&note.identifier)
            )),
            _ => return Vec::new(),
        }]
    }

    /// Raw HTML's content as phrasing (`htmlContent`). Elements and
    /// attributes stay inert; only text, images, links and media
    /// destinations survive.
    fn html(&self, raw: &str) -> Vec<Inline> {
        html::parse(raw)
            .iter()
            .flat_map(|node| self.convert(node, 0))
            .collect()
    }

    fn convert(&self, node: &HtmlNode, depth: usize) -> Vec<Inline> {
        let element = match node {
            HtmlNode::Text(text) => return vec![Inline::Text(text.clone())],
            HtmlNode::Raw(raw) => return vec![Inline::Text(raw.clone())],
            HtmlNode::Element(element) => element,
        };
        if depth > MAX_HTML_DEPTH {
            return vec![Inline::Text(html::outer_html(node))];
        }
        let name = element.name.as_str();
        if name == "img" {
            return vec![Inline::Image {
                url: self.url(element.attr("src").unwrap_or("")),
                alt: element.attr("alt").unwrap_or("").to_string(),
            }];
        }
        let mut children: Vec<Inline> = element
            .children
            .iter()
            .flat_map(|child| self.convert(child, depth + 1))
            .collect();
        let destination = element
            .attr(if name == "a" { "href" } else { "src" })
            .filter(|d| !d.is_empty());
        if let Some(destination) = destination
            && matches!(name, "a" | "video" | "audio" | "source")
        {
            let label = vec![Inline::Text(destination.to_string())];
            if name == "a" && !children.iter().any(|c| matches!(c, Inline::Link { .. })) {
                return vec![Inline::Link {
                    url: self.url(destination),
                    children: if children.is_empty() { label } else { children },
                }];
            }
            let mut out = vec![Inline::Link {
                url: self.url(destination),
                children: label,
            }];
            out.append(&mut children);
            return out;
        }
        match name {
            "br" | "hr" => vec![Inline::Text("\n".into())],
            // Adjacent table cells need a separator; rows already break.
            "td" | "th" => {
                children.push(Inline::Text(" ".into()));
                children
            }
            "details" | "summary" | "div" | "p" | "blockquote" | "li" | "tr" | "h1" | "h2"
            | "h3" | "h4" | "h5" | "h6" => {
                let mut out = vec![Inline::Text("\n".into())];
                out.append(&mut children);
                out.push(Inline::Text("\n".into()));
                out
            }
            _ => children,
        }
    }
}

/// A lone opening anchor tag yields its destination; anything else does
/// not.
fn anchor_href(raw: &str) -> Option<String> {
    let tag = raw.trim();
    let lower = tag.to_ascii_lowercase();
    let inner = lower.strip_prefix("<a")?.strip_suffix('>')?;
    if !(inner.is_empty() || inner.starts_with(char::is_whitespace)) || inner.contains(['<', '>']) {
        return None;
    }
    let nodes = html::parse(tag);
    html::find(&nodes, "a")?.attr("href").map(str::to_string)
}

fn closing_anchor(raw: &str) -> bool {
    let tag = raw.trim().to_ascii_lowercase();
    tag.strip_prefix("</a")
        .and_then(|rest| rest.strip_suffix('>'))
        .is_some_and(|rest| rest.trim().is_empty())
}

/// The label's plain text (`plainText`).
pub fn plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::Strong(c) | Inline::Emphasis(c) | Inline::Delete(c) => {
                out.push_str(&plain_text(c))
            }
            Inline::Link { children, .. } => out.push_str(&plain_text(children)),
            Inline::Image { .. } => {}
        }
    }
    out
}

/// Whether a link's label holds an image.
pub fn contains_image(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Image { .. } => true,
        Inline::Strong(c) | Inline::Emphasis(c) | Inline::Delete(c) => contains_image(c),
        Inline::Link { children, .. } => contains_image(children),
        _ => false,
    })
}

/// What a link or image shows (`media`): a player or preview for a
/// GitHub upload, else a link.
pub fn media(source: &str, metadata: &Metadata, image: bool) -> Option<Attachment> {
    attachments::attachment_for(source, metadata, image)
}

/// Whether a loaded player shows its label as a caption: links whose
/// label is more than the bare destination.
pub fn captioned(label: &[Inline], source: &str) -> bool {
    label.iter().any(|part| match part {
        Inline::Text(text) => text != source,
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::super::attachments::MediaKind;
    use super::*;

    const URL: &str = "https://github.com/block/buzz-app/pull/327";
    const BEFORE: &str =
        "https://github.com/user-attachments/assets/81e77cff-0e61-4694-9a88-add0c1f997e4";
    const AFTER: &str =
        "https://github.com/user-attachments/assets/dd1f7e4f-e2a4-4aec-b661-cb45032f29c5";
    const IMAGE: &str =
        "https://github.com/user-attachments/assets/cd0773ee-9861-46d9-8a0f-352ae951ec7f";
    const METADATA: &str = "<details><summary>before.mp4</summary><video src=\"https://private-user-images.githubusercontent.com/1888043/659873452-81e77cff-0e61-4694-9a88-add0c1f997e4.mp4?jwt=expired\"></video></details><video><source src=\"https://github.com/user-attachments/assets/dd1f7e4f-e2a4-4aec-b661-cb45032f29c5\" /></video>";

    fn blocks(body: &str) -> Vec<Block> {
        match prepare(body, None, URL) {
            Body::Rich { blocks, .. } => blocks,
            Body::Plain(_) => panic!("expected Markdown"),
        }
    }

    fn text(t: &str) -> Inline {
        Inline::Text(t.into())
    }

    fn link(url: &str, children: Vec<Inline>) -> Inline {
        Inline::Link {
            url: Some(url.into()),
            children,
        }
    }

    /// Every link in document order, as (label, destination).
    fn links(blocks: &[Block]) -> Vec<(String, Option<String>)> {
        fn inlines(list: &[Inline], out: &mut Vec<(String, Option<String>)>) {
            for inline in list {
                match inline {
                    Inline::Link { url, children } => {
                        out.push((plain_text(children), url.clone()));
                        inlines(children, out);
                    }
                    Inline::Strong(c) | Inline::Emphasis(c) | Inline::Delete(c) => inlines(c, out),
                    _ => {}
                }
            }
        }
        fn walk(blocks: &[Block], out: &mut Vec<(String, Option<String>)>) {
            for block in blocks {
                match block {
                    Block::Paragraph(i) | Block::Heading(i) => inlines(i, out),
                    Block::Quote(b) => walk(b, out),
                    Block::List { items, .. } => items.iter().for_each(|i| walk(&i.blocks, out)),
                    Block::Table { rows, .. } => {
                        rows.iter().flatten().for_each(|cell| inlines(cell, out))
                    }
                    Block::Footnotes(notes) => notes.iter().for_each(|n| walk(n, out)),
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(blocks, &mut out);
        out
    }

    fn all_text(blocks: &[Block]) -> String {
        blocks
            .iter()
            .map(|block| match block {
                Block::Paragraph(i) | Block::Heading(i) => plain_text(i),
                Block::Quote(b) => all_text(b),
                Block::Code(code) => code.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn renders_gfm_and_before_after_players() {
        let body = format!(
            "# Summary\n\n**Strong** and *emphasis* with [context](../328).\n\n- item\n- [x] done\n\n> quote\n\n| State | Result |\n| --- | --- |\n| Old | New |\n\n## Before\n\n{BEFORE}\n\n## After\n\n[Result recording]({AFTER})"
        );
        let Body::Rich { blocks, metadata } = prepare(&body, Some(METADATA), URL) else {
            panic!("expected Markdown")
        };
        assert_eq!(blocks[0], Block::Heading(vec![text("Summary")]));
        assert_eq!(
            blocks[1],
            Block::Paragraph(vec![
                Inline::Strong(vec![text("Strong")]),
                text(" and "),
                Inline::Emphasis(vec![text("emphasis")]),
                text(" with "),
                link(
                    "https://github.com/block/buzz-app/328",
                    vec![text("context")]
                ),
                text("."),
            ])
        );
        let Block::List { items, .. } = &blocks[2] else {
            panic!("expected a list")
        };
        assert_eq!(items[1].checked, Some(true));
        assert!(matches!(&blocks[3], Block::Quote(q) if all_text(q) == "quote"));
        let Block::Table { rows, .. } = &blocks[4] else {
            panic!("expected a table")
        };
        let cells: Vec<String> = rows.iter().flatten().map(|c| plain_text(c)).collect();
        assert_eq!(cells, ["State", "Result", "Old", "New"]);
        // Both uploads are videos, typed by the rendered HTML.
        let destinations = links(&blocks);
        let before = destinations
            .iter()
            .find(|(label, _)| label == BEFORE)
            .unwrap();
        assert_eq!(
            media(before.1.as_deref().unwrap(), &metadata, false).map(|a| a.kind),
            Some(MediaKind::Video)
        );
        let after = destinations
            .iter()
            .find(|(label, _)| label == "Result recording")
            .unwrap();
        assert_eq!(after.1.as_deref(), Some(AFTER));
        assert_eq!(
            media(AFTER, &metadata, false).map(|a| a.kind),
            Some(MediaKind::Video)
        );
        assert!(captioned(&[text("Result recording")], AFTER));
        assert!(!captioned(&[text(BEFORE)], BEFORE));
    }

    #[test]
    fn selects_images_audio_and_named_attachments() {
        let audio = "https://github.com/user-attachments/files/123/recording.wav";
        let body = format!(
            "![Screen description]({IMAGE})\n\n<img width=\"120\" alt=\"HTML image\" src=\"{IMAGE}\" onerror=\"alert(1)\">\n\n[Audio recording]({audio})\n\n[Movie]({AFTER})\n\n[Archive](https://github.com/user-attachments/files/99/source.zip)"
        );
        let html = format!("<a href=\"{AFTER}\">capture.mov</a><audio src=\"{audio}\"></audio>");
        let Body::Rich { blocks, metadata } = prepare(&body, Some(&html), URL) else {
            panic!("expected Markdown")
        };
        assert_eq!(
            blocks[0],
            Block::Paragraph(vec![Inline::Image {
                url: Some(IMAGE.into()),
                alt: "Screen description".into()
            }])
        );
        assert_eq!(
            blocks[1],
            Block::Paragraph(vec![Inline::Image {
                url: Some(IMAGE.into()),
                alt: "HTML image".into()
            }])
        );
        assert_eq!(
            media(audio, &metadata, false).map(|a| a.kind),
            Some(MediaKind::Audio)
        );
        assert_eq!(
            media(AFTER, &metadata, false).map(|a| a.kind),
            Some(MediaKind::Video)
        );
        assert_eq!(
            media(
                "https://github.com/user-attachments/files/99/source.zip",
                &metadata,
                false
            ),
            None
        );
    }

    #[test]
    fn preserves_html_only_summaries_prose_and_file_links() {
        let blocks = blocks(
            "<details><summary>Supporting evidence</summary><p>Read the logs &amp; notes.</p><a href=\"../files/report.zip\">Download report</a></details>",
        );
        let text = all_text(&blocks);
        let words: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(
            words.join(" "),
            "Supporting evidence Read the logs & notes. Download report"
        );
        assert_eq!(
            links(&blocks),
            [(
                "Download report".to_string(),
                Some("https://github.com/block/buzz-app/files/report.zip".to_string())
            )]
        );
    }

    #[test]
    fn keeps_mixed_html_text_images_and_links_in_order() {
        let blocks = blocks(&format!(
            "<div><p>Before the screenshot.</p><img src=\"{IMAGE}\" alt=\"HTML screenshot\" onerror=\"alert(1)\"><p>After the screenshot.</p><a href=\"https://github.com/user-attachments/files/99/source.zip\">Source archive</a></div>"
        ));
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected a paragraph")
        };
        let image = inlines
            .iter()
            .position(|i| matches!(i, Inline::Image { alt, .. } if alt == "HTML screenshot"))
            .unwrap();
        let before = inlines
            .iter()
            .position(|i| *i == text("Before the screenshot."))
            .unwrap();
        let after = inlines
            .iter()
            .position(|i| *i == text("After the screenshot."))
            .unwrap();
        assert!(before < image && image < after);
        assert_eq!(links(&blocks)[0].0, "Source archive");
    }

    #[test]
    fn retains_raw_media_destinations_without_metadata() {
        for body in [
            format!("<video src=\"{BEFORE}\">Recording fallback</video>"),
            format!("<video><source src=\"{BEFORE}\">Recording fallback</video>"),
            format!("<audio src=\"{BEFORE}\">Recording fallback</audio>"),
        ] {
            let blocks = blocks(&body);
            assert_eq!(
                links(&blocks)[0],
                (BEFORE.to_string(), Some(BEFORE.to_string()))
            );
            assert!(all_text(&blocks).contains("Recording fallback"));
        }
    }

    #[test]
    fn keeps_html_labels_while_rejecting_unsafe_destinations() {
        let blocks = blocks(
            "<div onclick=\"alert(1)\"><a href=\"javascript:alert(1)\">Unsafe action</a><a href=\"data:text/html,bad\">Unsafe data</a><a href=\"https://user:password@example.com/file\">Credential URL</a><a href=\"https://example.com/report.zip\" onclick=\"alert(1)\">Safe file</a></div>",
        );
        let text = all_text(&blocks);
        for label in ["Unsafe action", "Unsafe data", "Credential URL"] {
            assert!(text.contains(label));
        }
        let safe: Vec<_> = links(&blocks)
            .into_iter()
            .filter(|(_, url)| url.is_some())
            .collect();
        assert_eq!(
            safe,
            [(
                "Safe file".to_string(),
                Some("https://example.com/report.zip".to_string())
            )]
        );
    }

    #[test]
    fn keeps_inline_html_anchor_labels_linked() {
        let blocks = blocks("See <a href=\"https://example.com/docs\">the docs</a> now.");
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![
                text("See "),
                link("https://example.com/docs", vec![text("the docs")]),
                text(" now."),
            ])]
        );
    }

    #[test]
    fn separates_adjacent_html_table_cells() {
        let blocks = blocks(
            "<table><tr><th>Before</th><th>After</th></tr><tr><td>old</td><td>new</td></tr></table>",
        );
        let text = all_text(&blocks);
        assert!(text.contains("Before After"));
        assert!(text.contains("old new"));
    }

    #[test]
    fn preserves_single_line_breaks_alongside_blocks() {
        let blocks = blocks(
            "First requirement.\nSecond requirement.\nThird requirement.\n\nSeparate paragraph.\n\n- one\n- two\n\n```\nline one\nline two\n```",
        );
        assert_eq!(
            blocks[0],
            Block::Paragraph(vec![text(
                "First requirement.\nSecond requirement.\nThird requirement."
            )])
        );
        assert_eq!(
            blocks[1],
            Block::Paragraph(vec![text("Separate paragraph.")])
        );
        let Block::List { items, spread, .. } = &blocks[2] else {
            panic!("expected a list")
        };
        assert!(!spread);
        assert_eq!(
            items
                .iter()
                .map(|i| all_text(&i.blocks))
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(blocks[3], Block::Code("line one\nline two".into()));
    }

    #[test]
    fn never_interprets_literal_code_or_unsafe_links() {
        let body = format!(
            "`{BEFORE}`\n\n```html\n<img src=\"{IMAGE}\">\n{AFTER}\n```\n\n<script>alert(1)</script>\n\n<iframe src=\"https://evil.test\"></iframe>\n\n[Bad](javascript:alert%281%29) ![Unsafe](data:image/svg+xml,bad)\n\n<img src=\"https://github.com.evil.test/image.png\" alt=\"External\">"
        );
        let Body::Rich { blocks, metadata } = prepare(&body, Some(METADATA), URL) else {
            panic!("expected Markdown")
        };
        assert_eq!(
            blocks[0],
            Block::Paragraph(vec![Inline::Code(BEFORE.into())])
        );
        assert!(matches!(&blocks[1], Block::Code(code) if code.contains(AFTER)));
        assert_eq!(
            blocks[4],
            Block::Paragraph(vec![
                Inline::Link {
                    url: None,
                    children: vec![text("Bad")]
                },
                text(" "),
                Inline::Image {
                    url: None,
                    alt: "Unsafe".into()
                },
            ])
        );
        // An image off GitHub's hosts is a link, not a preview.
        let Block::Paragraph(external) = &blocks[5] else {
            panic!("expected a paragraph")
        };
        let Inline::Image {
            url: Some(external),
            ..
        } = &external[0]
        else {
            panic!("expected an image")
        };
        assert_eq!(media(external, &metadata, true), None);
    }

    #[test]
    fn keeps_linked_images_out_of_anchors() {
        for content in [
            format!("![Linked image]({IMAGE})"),
            format!("**![Linked image]({IMAGE})**"),
        ] {
            let blocks = blocks(&format!("[{content}](https://example.com/review)"));
            let Block::Paragraph(inlines) = &blocks[0] else {
                panic!("expected a paragraph")
            };
            let Inline::Link { url, children } = &inlines[0] else {
                panic!("expected a link")
            };
            assert_eq!(url.as_deref(), Some("https://example.com/review"));
            assert!(contains_image(children));
        }
    }

    #[test]
    fn preserves_linked_html_images_and_media_fallbacks() {
        let blocks = blocks(&format!(
            "<div><a href=\"https://example.com/review\"><img src=\"{IMAGE}\" alt=\"Linked HTML image\"></a><a href=\"https://example.com/recording\"><video src=\"{BEFORE}\">Recording</video></a></div>"
        ));
        let destinations: Vec<_> = links(&blocks)
            .into_iter()
            .filter_map(|(_, url)| url)
            .collect();
        assert_eq!(
            destinations,
            [
                "https://example.com/review",
                "https://example.com/recording",
                BEFORE
            ]
        );
    }

    #[test]
    fn keeps_deep_html_as_literal_text() {
        let blocks = blocks(&format!(
            "{}Deep evidence<img src=\"{IMAGE}\">{}",
            "<div>".repeat(MAX_HTML_DEPTH + 2),
            "</div>".repeat(MAX_HTML_DEPTH + 2)
        ));
        let text = all_text(&blocks);
        assert!(text.contains(&format!("Deep evidence<img src=\"{IMAGE}\">")));
    }

    #[test]
    fn falls_back_to_literal_text_over_budget() {
        for body in ["x".repeat(100_001), format!("{}deep", "> ".repeat(101))] {
            assert_eq!(
                prepare(&body, Some(METADATA), URL),
                Body::Plain(body.clone())
            );
        }
    }
}
