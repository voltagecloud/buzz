//! Message bodies (ui.md §4.4 body text, §4.6), after buzz-app's
//! `MessageMarkdown.tsx`.
//!
//! Blocks are laid out here, to the spec's margins, quote rule, list
//! gutters, code block and table frames. gpui-kit's `TextView` hard-codes
//! those, so it only lays out inline text: each paragraph, heading, list
//! line and table cell is re-serialized from the parsed tree into a
//! one-paragraph Markdown source for a `TextView`, which wraps it, keeps it
//! selectable and places links, mentions, custom emoji, spoilers, footnote
//! marks and code spans as native inline elements through a
//! [`MarkdownPlugin`].
//!
//! Preparing a body repairs pasted wrapped links, binds names to signed
//! recipients, finds custom emoji, bare Buzz links and `#channel` names in
//! prose only (never in code or link labels), pairs `||spoiler||` marks
//! across formatting, turns single newlines into hard breaks (remark-breaks)
//! and shows raw HTML as the text the author typed (`remarkLiteralHtml`).
//! Bodies over the size or depth limits render as plain text.

use super::identity::{self, AgentLibrary, Names};
use super::links::{self, Chip, StyledLabel};
use super::{media, navigation};
use crate::community::Community;
use buzz_relay_client::fold::{CustomEmoji, MessageRow, message_markdown, visible_html_source};
use buzz_relay_client::{ChannelSummary, Media, Profiles, Session};
use buzz_ui::{ActiveTheme, Icon, IconName, Tooltip, TypeStyled, tokens::font};
use gpui_kit::base::SelectableText;
use gpui_kit::base::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, TextView, TextViewStyle,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use markdown::mdast::{self, AlignKind, Node};
use nostr::prelude::PublicKey;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Everything outside the row a body needs to render.
#[derive(Clone)]
pub struct Directory {
    pub profiles: Profiles,
    pub channels: Arc<[ChannelSummary]>,
    pub media: Media,
    /// Display names, with the viewer's agents named from their library.
    pub names: Arc<Names>,
    /// For lookups a body starts: channels behind links, missing profiles.
    pub session: Option<Arc<Session>>,
    /// The agent names and channels, hashed once for each body's cache key.
    fingerprint: u64,
}

impl Directory {
    /// A directory without the agent library or a session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(profiles: Profiles, channels: Arc<[ChannelSummary]>, media: Media) -> Self {
        let names = identity::names(&profiles, &AgentLibrary::default(), "", None, None);
        Self {
            fingerprint: fingerprint(&names, &channels),
            profiles,
            channels,
            media,
            names,
            session: None,
        }
    }

    /// The community's directory, names scoped to `members` when given.
    pub fn of(community: &Community, members: Option<&[String]>, cx: &App) -> Self {
        let profiles = community.snapshot.profiles.clone();
        let library = AgentLibrary::from_hosts(cx);
        let names = identity::names(
            &profiles,
            &library,
            community.session.viewer(),
            Some(&community.origin),
            members,
        );
        let channels = community.snapshot.channels.clone();
        Self {
            fingerprint: fingerprint(&names, &channels),
            profiles,
            channels,
            media: community.session.media().clone(),
            names,
            session: Some(community.session.clone()),
        }
    }

    pub fn viewer(&self) -> &str {
        self.names.viewer()
    }

    /// Reads these people's profiles if they are not known yet.
    pub fn want_profiles(&self, people: Vec<String>) {
        let missing: Vec<String> = people
            .into_iter()
            .filter(|p| !self.profiles.contains_key(p))
            .collect();
        if let (Some(session), false) = (&self.session, missing.is_empty()) {
            session.want_profiles(missing);
        }
    }
}

thread_local! {
    /// Bodies rendered now take the subtle text colour.
    static SUBTLE: Cell<bool> = const { Cell::new(false) };
}

/// Renders the bodies `f` builds in the subtle text colour, as an open
/// quiet line shows its message.
pub(super) fn subtle<T>(f: impl FnOnce() -> T) -> T {
    let was = SUBTLE.replace(true);
    let out = f();
    SUBTLE.set(was);
    out
}

/// The colour of body text outside quotes.
fn body_text(c: &buzz_ui::Colors) -> Hsla {
    if SUBTLE.get() {
        c.text_subtle
    } else {
        c.text_standard
    }
}

thread_local! {
    /// The body being drawn sits in a thread pane.
    static IN_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The body being drawn is no message's (`text`).
    static NO_ORIGIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Draws with `thread` saying whether the place is a thread pane, which
/// decides whether a link adds a panel or replaces earlier ones.
pub fn in_place<T>(thread: bool, draw: impl FnOnce() -> T) -> T {
    let before = IN_THREAD.replace(thread);
    let drawn = draw();
    IN_THREAD.set(before);
    drawn
}

/// Renders a row's body.
pub fn body(row: &Arc<MessageRow>, directory: &Directory, cx: &App) -> AnyElement {
    let document = prepared(row, directory);
    let c = cx.colors();
    let id = row.id.to_hex();
    // The body's width, so inline chips and spoilers can wrap inside it.
    let width = measured("body-width", &id);
    let objects = Objects {
        directory: directory.clone(),
        emoji: row.emoji.clone().into(),
        row: id.clone().into(),
        origin: (!NO_ORIGIN.get()).then(|| navigation::LinkOrigin {
            thread: IN_THREAD.get(),
            ..navigation::LinkOrigin::of(row)
        }),
        spoilers: document.spoilers.clone(),
        max_width: (width.get() < f32::MAX).then(|| px(width.get())),
        link_mode: links::mode(),
        plain: super::renderers::plain(),
    };
    let content = match &document.body {
        Document::Plain(text) => div()
            .when(!SUBTLE.get(), |d| d.text_message())
            .text_color(body_text(c))
            .child(SelectableText::new(
                SharedString::from(format!("plain-{id}")),
                SharedString::from(text.clone()),
            ))
            .into_any_element(),
        Document::Emoji(parts) => jumbo(parts, &id, &directory.media),
        Document::Blocks(blocks) => {
            let mut out = Out {
                objects,
                cx,
                quote: false,
            };
            column(blocks, Container::Root, "b", &mut out).into_any_element()
        }
    };
    div()
        .relative()
        .w_full()
        .child(measure(
            width,
            |bounds| bounds.size.width.into(),
            |_, _| true,
        ))
        .child(content)
        .into_any_element()
}

/// What the last frame measured of an element, by `kind` and id: the
/// canvas from [`measure`] writes it, and the next render reads it. It
/// starts at `f32::MAX`.
pub(super) fn measured(kind: &'static str, id: &str) -> Rc<Cell<f32>> {
    thread_local! {
        static MEASURED: RefCell<HashMap<String, Rc<Cell<f32>>>> =
            RefCell::new(HashMap::new());
    }
    MEASURED.with_borrow_mut(|measured| {
        if measured.len() > 8192 {
            measured.clear();
        }
        measured
            .entry(format!("{kind}:{id}"))
            .or_insert_with(|| Rc::new(Cell::new(f32::MAX)))
            .clone()
    })
}

/// A canvas over its parent that writes `read(bounds)` into `cell` each
/// frame, and redraws the window when `matters(before, now)`.
pub(super) fn measure(
    cell: Rc<Cell<f32>>,
    read: fn(&Bounds<Pixels>) -> f32,
    matters: fn(f32, f32) -> bool,
) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let (before, now) = (cell.get(), read(&bounds));
            if (before - now).abs() > 0.5 {
                cell.set(now);
                if matters(before, now) {
                    window.refresh();
                }
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// Markdown that is not a message, such as a README or a plugin's text,
/// with Buzz links routed through navigation, rendered by the message
/// renderer; too long or too deep text stays plain. `channel` names
/// where it shows.
pub fn text(text: &str, channel: &str, community: &Community, cx: &App) -> AnyElement {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let hash = hasher.finish().to_be_bytes();
    let mut id = [0u8; 32];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = hash[index % 8] ^ (index as u8);
    }
    let row = Arc::new(MessageRow {
        id: buzz_relay_client::EventId::from_byte_array(id),
        channel: Arc::from(channel),
        kind: 9,
        audience: None,
        author: nostr::prelude::PublicKey::from_byte_array([1; 32]),
        signer: None,
        workflow_owner: None,
        created_at: 0,
        created_at_ms: 0,
        content: text.to_string(),
        source_content: None,
        edited: false,
        attachment_seams: Vec::new(),
        attachment_source_id: None,
        attachments: Vec::new(),
        emoji: Vec::new(),
        mentions: Vec::new(),
        mention_references: Vec::new(),
        reactions: Vec::new(),
        reply_count: 0,
        participants: Vec::new(),
        thread_root_id: None,
        reply_parent_id: None,
        sent_from_thread: None,
        diff: None,
        membership: None,
        delivery: None,
    });
    // No message: its links open with no origin.
    NO_ORIGIN.set(true);
    let drawn = body(&row, &Directory::of(community, None, cx), cx);
    NO_ORIGIN.set(false);
    drawn
}

/// One link chip for `url` outside a body, such as the "Sent from" line.
pub fn link_chip(url: &str, label: &str, directory: &Directory, id: &str, _cx: &App) -> AnyElement {
    let (url, label, directory, id) = (
        url.to_string(),
        label.to_string(),
        directory.clone(),
        SharedString::from(id.to_string()),
    );
    // The chip uses window state; a deferred element builds it in place.
    LinkChipElement {
        id,
        url,
        label,
        directory,
        mode: links::mode(),
    }
    .into_any_element()
}

#[derive(IntoElement)]
struct LinkChipElement {
    id: SharedString,
    url: String,
    label: String,
    directory: Directory,
    mode: links::LinkMode,
}

impl RenderOnce for LinkChipElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let size = window.text_style().font_size.to_pixels(window.rem_size());
        let mode = self.mode;
        links::with_mode(mode, || {
            links::chip(
                Chip {
                    id: self.id,
                    url: &self.url,
                    label: Some(StyledLabel {
                        text: self.label,
                        runs: Vec::new(),
                    }),
                    directory: &self.directory,
                    font_size: size,
                    line_height: px(16.),
                    max_width: None,
                    origin: None,
                },
                window,
                cx,
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Preparation

#[derive(Debug, PartialEq)]
struct Prepared {
    body: Document,
    /// Each spoiler's inner one-paragraph Markdown, by index.
    spoilers: Arc<[SharedString]>,
}

#[derive(Debug, PartialEq)]
enum Document {
    Plain(String),
    /// Emoji only (`emoji-size.ts`): drawn at 42px.
    Emoji(Vec<EmojiPart>),
    Blocks(Vec<Block>),
}

#[derive(Debug, PartialEq)]
enum EmojiPart<E = CustomEmoji> {
    Text(String),
    Custom(E),
}

#[derive(Debug, PartialEq)]
enum Block {
    /// One-paragraph Markdown for a `TextView`.
    Paragraph(SharedString),
    Heading(u8, SharedString),
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u32,
        items: Vec<Item>,
    },
    Code(String),
    Table {
        align: Vec<AlignKind>,
        /// One-paragraph Markdown per cell.
        rows: Vec<Vec<SharedString>>,
    },
    Rule,
    /// GFM footnote definitions, in reference order.
    Footnotes(Vec<Vec<Block>>),
}

#[derive(Debug, PartialEq)]
struct Item {
    checked: Option<bool>,
    blocks: Vec<Block>,
}

thread_local! {
    /// Prepared bodies by their inputs, so a redraw does not reparse.
    static PREPARED: RefCell<HashMap<u64, Arc<Prepared>>> = RefCell::new(HashMap::new());
}
const PREPARED_LIMIT: usize = 2048;

fn prepared(row: &MessageRow, directory: &Directory) -> Arc<Prepared> {
    let identities = identities(row);
    let mut hasher = DefaultHasher::new();
    // Text renderers may draw the same prose differently by message.
    row.id.hash(&mut hasher);
    row.content.hash(&mut hasher);
    row.edited.hash(&mut hasher);
    row.attachment_seams.hash(&mut hasher);
    row.emoji.hash(&mut hasher);
    for key in &identities {
        key.hash(&mut hasher);
        directory
            .profiles
            .get(key)
            .map(|p| &p.name)
            .hash(&mut hasher);
    }
    directory.fingerprint.hash(&mut hasher);
    // Plugins' text renderers change what prose draws, unless they are
    // left out.
    super::renderers::text_generation().hash(&mut hasher);
    super::renderers::plain().hash(&mut hasher);
    // With Emoji off, custom emoji are plain text plugins may draw.
    crate::plugins::enabled("buzz.emoji").hash(&mut hasher);
    let key = hasher.finish();
    if let Some(document) = PREPARED.with_borrow(|cache| cache.get(&key).cloned()) {
        return document;
    }
    let document = Arc::new(prepare(row, directory));
    PREPARED.with_borrow_mut(|cache| {
        if cache.len() >= PREPARED_LIMIT {
            cache.clear();
        }
        cache.insert(key, document.clone());
    });
    document
}

/// What a body reads from the directory besides its own recipients'
/// profiles: agent names and channel links.
fn fingerprint(names: &Names, channels: &[ChannelSummary]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut agents: Vec<(&str, &str)> = names.agent_names().collect();
    agents.sort();
    agents.hash(&mut hasher);
    for channel in channels {
        channel.id.hash(&mut hasher);
        channel.name.hash(&mut hasher);
        channel.archived.hash(&mut hasher);
        channel.private.hash(&mut hasher);
    }
    hasher.finish()
}

fn identities(row: &MessageRow) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for key in row.mentions.iter().chain(&row.mention_references) {
        let hex = key.to_hex();
        if !keys.contains(&hex) {
            keys.push(hex);
        }
    }
    keys
}

/// Node types whose text is never prose (`isLiteralMarkdownContext`).
fn literal(node: &Node) -> bool {
    matches!(
        node,
        Node::Code(_)
            | Node::InlineCode(_)
            | Node::Link(_)
            | Node::LinkReference(_)
            | Node::Image(_)
            | Node::ImageReference(_)
            | Node::Definition(_)
            | Node::Html(_)
    )
}

pub(super) fn literal_ranges(node: &Node, include_links: bool, out: &mut Vec<(usize, usize)>) {
    if literal(node) && (include_links || !matches!(node, Node::Link(_))) {
        if let Some(position) = node.position() {
            out.push((position.start.offset, position.end.offset));
        }
        return;
    }
    for child in node.children().into_iter().flatten() {
        literal_ranges(child, include_links, out);
    }
}

/// A name a mention can bind, with the identities it names.
pub(super) type MentionLabel = (String, Vec<PublicKey>);

/// `profileMentionParts`' labels: the exact profile and agent names of the
/// row's signed recipients, longest first. A name two of them share binds
/// neither.
pub(super) fn mention_labels(
    row: &MessageRow,
    profiles: &Profiles,
    names: &Names,
) -> Vec<MentionLabel> {
    let library: HashMap<&str, &str> = names.agent_names().collect();
    let mut labels: Vec<MentionLabel> = Vec::new();
    for key in row.mentions.iter().chain(&row.mention_references) {
        let hex = key.to_hex();
        let candidates = [
            profiles.get(&hex).map(|p| p.name.clone()),
            library.get(hex.as_str()).map(|name| name.to_string()),
        ];
        for name in candidates.into_iter().flatten() {
            if name.is_empty() || name.contains(['\r', '\n']) {
                continue;
            }
            match labels.iter_mut().find(|(n, _)| *n == name) {
                Some((_, keys)) if !keys.contains(key) => keys.push(*key),
                Some(_) => {}
                None => labels.push((name, vec![*key])),
            }
        }
    }
    labels.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    labels
}

/// The bound name an `@` at byte `i` of `text` starts: not after a word,
/// path or another `@`, ending at a word boundary, and not across an
/// attachment seam (`spans_seam(start, end)`). Gives its end and keys.
pub(super) fn mention_at<'a>(
    text: &str,
    i: usize,
    labels: &'a [MentionLabel],
    spans_seam: impl Fn(usize, usize) -> bool,
) -> Option<(usize, &'a [PublicKey])> {
    let rest = text[i..].strip_prefix('@')?;
    let previous = text[..i].chars().next_back();
    if previous.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '@' | '/' | '\\')) {
        return None;
    }
    labels.iter().find_map(|(name, keys)| {
        let end = i + 1 + name.len();
        (rest.starts_with(name.as_str())
            && !text[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !spans_seam(i, end))
        .then_some((end, keys.as_slice()))
    })
}

/// The raw body with each mention bound to one identity written by
/// `link(label, key, out)`, outside `literals` (code and other literal
/// syntax) and never across a `seam`. Other text is kept as is.
pub(super) fn bind_mentions(
    text: &str,
    labels: &[MentionLabel],
    literals: &[(usize, usize)],
    seams: &[usize],
    link: impl Fn(&str, &PublicKey, &mut String),
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((end, keys)) = mention_at(text, i, labels, |start, end| {
            seams.iter().any(|s| start < *s && *s < end)
        }) {
            let inside = literals.iter().any(|&(s, e)| i < e && end > s);
            match keys {
                [key] if !inside => link(&text[i..end], key, &mut out),
                _ => out.push_str(&text[i..end]),
            }
            i = end;
            continue;
        }
        let c = text[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn prepare(row: &MessageRow, directory: &Directory) -> Prepared {
    let plain = |body| Prepared {
        body,
        spoilers: Arc::from([]),
    };
    if is_emoji_only(&row.content, &row.emoji) {
        return plain(Document::Emoji(emoji_parts(row.content.trim(), &row.emoji)));
    }
    let Some(mut tree) = message_markdown(&row.content) else {
        return plain(Document::Plain(row.content.clone()));
    };
    // `normalizeWrappedLinks`, outside code and other literal syntax.
    let mut literals = Vec::new();
    literal_ranges(&tree, false, &mut literals);
    let normalized = normalize_wrapped_links(&row.content, |start, end| {
        literals.iter().any(|&(s, e)| start < e && end > s)
    });
    let content: &str = if normalized != row.content {
        match message_markdown(&normalized) {
            Some(repaired) => {
                tree = repaired;
                &normalized
            }
            None => return plain(Document::Plain(row.content.clone())),
        }
    } else {
        &row.content
    };
    // `profileMentionParts`: exact known names of signed recipients, only
    // in an unedited body (link-repaired prose stays unbound, like edits).
    let names = if !row.edited && content == row.content {
        mention_labels(row, &directory.profiles, &directory.names)
    } else {
        Vec::new()
    };
    let mut definitions = HashMap::new();
    let mut footnotes = HashMap::new();
    collect_definitions(&tree, &mut definitions, &mut footnotes);
    let mut prep = Prep {
        message: super::renderers::InMessage {
            row,
            profiles: &directory.profiles,
            reaction: None,
        },
        content,
        emoji: &row.emoji,
        names,
        seams: &row.attachment_seams,
        definitions,
        channels: channel_references(&directory.channels),
        footnote_definitions: footnotes,
        footnote_order: Vec::new(),
        spoilers: Vec::new(),
        in_link: false,
    };
    let mut blocks = prep.blocks(tree.children().map_or(&[][..], Vec::as_slice));
    // Footnotes follow the body, in the order they were first referenced.
    let mut notes = Vec::new();
    let mut index = 0;
    while index < prep.footnote_order.len() {
        let id = prep.footnote_order[index].clone();
        if let Some(children) = prep.footnote_definitions.get(&id).cloned() {
            let mut note = prep.blocks(&children);
            match note.last_mut() {
                Some(Block::Paragraph(text)) => {
                    *text = format!("{text} [↩](<buzz-footnote-back:{}>)", index + 1).into()
                }
                _ => note.push(Block::Paragraph(
                    format!("[↩](<buzz-footnote-back:{}>)", index + 1).into(),
                )),
            }
            notes.push(note);
        }
        index += 1;
    }
    if !notes.is_empty() {
        blocks.push(Block::Footnotes(notes));
    }
    Prepared {
        body: Document::Blocks(blocks),
        spoilers: prep.spoilers.into_iter().map(SharedString::from).collect(),
    }
}

fn collect_definitions(
    node: &Node,
    links: &mut HashMap<String, String>,
    notes: &mut HashMap<String, Vec<Node>>,
) {
    match node {
        Node::Definition(definition) => {
            links
                .entry(definition.identifier.to_lowercase())
                .or_insert_with(|| definition.url.clone());
        }
        Node::FootnoteDefinition(note) => {
            notes
                .entry(note.identifier.to_lowercase())
                .or_insert_with(|| note.children.clone());
            return;
        }
        _ => {}
    }
    for child in node.children().into_iter().flatten() {
        collect_definitions(child, links, notes);
    }
}

/// `messageReferences` for channels: `#name` of a listed, unarchived
/// channel, when no other listed channel shares the name. Longest first.
fn channel_references(channels: &[ChannelSummary]) -> Vec<(String, String)> {
    let candidates: Vec<(String, String)> = channels
        .iter()
        .filter(|c| !c.archived)
        .map(|c| (format!("#{}", c.name), c.id.clone()))
        .collect();
    let mut unique: Vec<(String, String)> = candidates
        .iter()
        .filter(|(label, id)| !candidates.iter().any(|(l, i)| l == label && i != id))
        .cloned()
        .collect();
    unique.sort_by_key(|(label, _)| std::cmp::Reverse(label.len()));
    unique
}

/// A piece of inline content after spoiler pairing (`remarkSpoilers`).
enum Piece<'a> {
    Node(&'a Node),
    /// Text split out of a text node, with its source offset when known.
    Text(String, Option<usize>),
    Spoiler(Vec<Piece<'a>>),
}

struct Prep<'a> {
    /// The message, for text renderers.
    message: super::renderers::InMessage<'a>,
    content: &'a str,
    emoji: &'a [CustomEmoji],
    names: Vec<MentionLabel>,
    seams: &'a [usize],
    definitions: HashMap<String, String>,
    /// `(#label, channel id)`.
    channels: Vec<(String, String)>,
    footnote_definitions: HashMap<String, Vec<Node>>,
    footnote_order: Vec<String>,
    spoilers: Vec<String>,
    /// Inside a link label: text is escaped and nothing more.
    in_link: bool,
}

impl Prep<'_> {
    fn blocks(&mut self, nodes: &[Node]) -> Vec<Block> {
        nodes.iter().filter_map(|node| self.block(node)).collect()
    }

    fn block(&mut self, node: &Node) -> Option<Block> {
        Some(match node {
            Node::Paragraph(paragraph) => {
                let text = self.inline(&paragraph.children, true);
                if text.trim().is_empty() {
                    return None;
                }
                Block::Paragraph(text.into())
            }
            Node::Heading(heading) => {
                Block::Heading(heading.depth, self.inline(&heading.children, true).into())
            }
            Node::Blockquote(quote) => Block::Quote(self.blocks(&quote.children)),
            Node::List(list) => Block::List {
                ordered: list.ordered,
                start: list.start.unwrap_or(1),
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
                    .map(|row| {
                        row.children()
                            .into_iter()
                            .flatten()
                            .map(|cell| {
                                // Table syntax is grammar, never a spoiler.
                                let children = cell.children().map_or(&[][..], Vec::as_slice);
                                SharedString::from(self.inline(children, false))
                            })
                            .collect()
                    })
                    .collect(),
            },
            Node::ThematicBreak(_) => Block::Rule,
            // Raw HTML among blocks is a paragraph of its source.
            Node::Html(html) => {
                let mut text = String::new();
                literal_html(&html.value, &mut text);
                if text.trim().is_empty() {
                    return None;
                }
                Block::Paragraph(text.into())
            }
            // Definitions only resolve references.
            _ => return None,
        })
    }

    /// One-paragraph Markdown for `nodes`, every ASCII punctuation mark of
    /// prose escaped so nothing but the structure written here parses.
    fn inline(&mut self, nodes: &[Node], spoilers: bool) -> String {
        let pieces = if spoilers {
            self.pair_spoilers(nodes)
        } else {
            nodes.iter().map(Piece::Node).collect()
        };
        let mut out = String::new();
        self.pieces(&pieces, &mut out);
        out.trim_end_matches(['\\', '\n']).to_string()
    }

    fn pieces(&mut self, pieces: &[Piece], out: &mut String) {
        for piece in pieces {
            match piece {
                Piece::Node(node) => self.inline_node(node, out),
                Piece::Text(value, start) => self.text(value, *start, out),
                Piece::Spoiler(inner) => {
                    let mut markdown = String::new();
                    self.pieces(inner, &mut markdown);
                    let plain = plain_markdown(&markdown);
                    let index = self.spoilers.len();
                    self.spoilers.push(markdown);
                    image_md(&plain, &format!("buzz-spoiler:{index}"), out);
                }
            }
        }
    }

    /// `remarkSpoilers` over one parent's children: `||` in text nodes
    /// opens and closes spoilers that may hold sibling formatting. An empty
    /// pair and an unclosed mark stay literal.
    fn pair_spoilers<'n>(&self, nodes: &'n [Node]) -> Vec<Piece<'n>> {
        let mut output: Vec<Piece<'n>> = Vec::new();
        let mut hidden: Option<Vec<Piece<'n>>> = None;
        for node in nodes {
            let Node::Text(text) = node else {
                match &mut hidden {
                    Some(hidden) => hidden.push(Piece::Node(node)),
                    None => output.push(Piece::Node(node)),
                }
                continue;
            };
            let delimiters = self.delimiters(&text.value, node);
            if delimiters.is_empty() {
                match &mut hidden {
                    Some(hidden) => hidden.push(Piece::Node(node)),
                    None => output.push(Piece::Node(node)),
                }
                continue;
            }
            let base = self.source_start(&text.value, node);
            let mut from = 0;
            for (index, at) in delimiters
                .iter()
                .chain(std::iter::once(&text.value.len()))
                .enumerate()
            {
                let value = &text.value[from..*at];
                if index > 0 {
                    match hidden.take() {
                        Some(inner) if !inner.is_empty() => output.push(Piece::Spoiler(inner)),
                        Some(_) => output.push(Piece::Text("||||".into(), None)),
                        None => hidden = Some(Vec::new()),
                    }
                }
                if !value.is_empty() {
                    let piece = Piece::Text(value.to_string(), base.map(|b| b + from));
                    match &mut hidden {
                        Some(hidden) => hidden.push(piece),
                        None => output.push(piece),
                    }
                }
                from = (*at + 2).min(text.value.len());
            }
        }
        if let Some(hidden) = hidden {
            output.push(Piece::Text("||".into(), None));
            output.extend(hidden);
        }
        output
    }

    /// Where `||` marks really are in a text node's value: in the source,
    /// not produced by an escape.
    fn delimiters(&self, value: &str, node: &Node) -> Vec<usize> {
        if !value.contains("||") {
            return Vec::new();
        }
        let Some(position) = node.position() else {
            return Vec::new();
        };
        let Some(source) = self.content.get(position.start.offset..position.end.offset) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let (bytes, mut i, mut v) = (source.as_bytes(), 0, 0);
        while i < bytes.len() && v <= value.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() {
                i += 2;
                v += 1;
                continue;
            }
            if bytes[i] == b'&' {
                // Entities change lengths; give up on mapping.
                return if source == value { found } else { Vec::new() };
            }
            if source[i..].starts_with("||") && value.get(v..v + 2) == Some("||") {
                found.push(v);
                i += 2;
                v += 2;
                continue;
            }
            let c = source[i..].chars().next().expect("in bounds");
            i += c.len_utf8();
            v += c.len_utf8();
        }
        found
    }

    fn source_start(&self, value: &str, node: &Node) -> Option<usize> {
        let position = node.position()?;
        let source = self
            .content
            .get(position.start.offset..position.end.offset)?;
        (source == value).then_some(position.start.offset)
    }

    fn inline_node(&mut self, node: &Node, out: &mut String) {
        match node {
            Node::Text(text) => {
                let start = self.source_start(&text.value, node);
                self.text(&text.value, start, out)
            }
            Node::Emphasis(e) => self.wrapped("*", &e.children, out),
            Node::Strong(s) => self.wrapped("**", &s.children, out),
            Node::Delete(d) => self.wrapped("~~", &d.children, out),
            Node::InlineCode(code) => code_span(&code.value, out),
            Node::Break(_) => out.push_str("\\\n"),
            Node::Link(link) => {
                let url = link.url.clone();
                self.link(&link.children, &url, out)
            }
            Node::LinkReference(link) => {
                match self
                    .definitions
                    .get(&link.identifier.to_lowercase())
                    .cloned()
                {
                    Some(url) => self.link(&link.children, &url, out),
                    None => escape(&plain_text(&link.children), out),
                }
            }
            Node::Image(image) => image_md(&image.alt, &image.url, out),
            Node::ImageReference(image) => {
                let url = self
                    .definitions
                    .get(&image.identifier.to_lowercase())
                    .cloned()
                    .unwrap_or_default();
                image_md(&image.alt, &url, out);
            }
            Node::FootnoteReference(note) => {
                let id = note.identifier.to_lowercase();
                if !self.footnote_definitions.contains_key(&id) {
                    escape(&format!("[^{}]", note.identifier), out);
                    return;
                }
                let number = match self.footnote_order.iter().position(|n| *n == id) {
                    Some(index) => index + 1,
                    None => {
                        self.footnote_order.push(id);
                        self.footnote_order.len()
                    }
                };
                image_md(&number.to_string(), &format!("buzz-footnote:{number}"), out);
            }
            Node::Html(html) => literal_html(&html.value, out),
            other => escape(&other.to_string(), out),
        }
    }

    /// A link keeps its label's formatting; text inside is only escaped.
    fn link(&mut self, children: &[Node], url: &str, out: &mut String) {
        let was = std::mem::replace(&mut self.in_link, true);
        let mut inner = String::new();
        for child in children {
            self.inline_node(child, &mut inner);
        }
        self.in_link = was;
        let inner = inner.trim_end_matches(['\\', '\n']);
        out.push('[');
        out.push_str(inner);
        out.push_str("](<");
        out.push_str(&destination(url));
        out.push_str(">)");
    }

    fn wrapped(&mut self, marker: &str, children: &[Node], out: &mut String) {
        let mut inner = String::new();
        let pieces = if self.in_link {
            children.iter().map(Piece::Node).collect()
        } else {
            self.pair_spoilers(children)
        };
        self.pieces(&pieces, &mut inner);
        if inner.is_empty() {
            return;
        }
        out.push_str(marker);
        out.push_str(&inner);
        out.push_str(marker);
    }

    /// Prose: bound mentions, custom emoji, bare and wrapped links, channel
    /// names and hard breaks.
    fn text(&mut self, value: &str, start: Option<usize>, out: &mut String) {
        if self.in_link {
            for c in value.chars() {
                match c {
                    '\n' | '\r' => out.push(' '),
                    c => escape_char(c, out),
                }
            }
            return;
        }
        // Seams index the body; map them when the node's source is its value.
        let spans_seam = |from: usize, to: usize| match start {
            Some(base) => self
                .seams
                .iter()
                .any(|seam| base + from < *seam && *seam < base + to),
            None => !self.seams.is_empty(),
        };
        let emoji_on = crate::plugins::enabled("buzz.emoji");
        // Plugins' text renderers see the plain runs between mentions,
        // links and channel names, as `InlineText` does; custom emoji stay
        // in the run.
        let mut drawn = Vec::new();
        {
            let mut runs = Vec::new();
            let (mut from, mut j) = (0, 0);
            while j < value.len() {
                let end = mention_at(value, j, &self.names, spans_seam)
                    .map(|(end, _)| end)
                    .or_else(|| link_part(value, j).map(|found| found.end))
                    .or_else(|| self.channel_at(value, j).map(|(label, _)| j + label.len()));
                match end {
                    Some(end) => {
                        runs.push(from..j);
                        (from, j) = (end, end);
                    }
                    None => j += value[j..].chars().next().map_or(1, char::len_utf8),
                }
            }
            runs.push(from..value.len());
            for run in runs.into_iter().filter(|run| !run.is_empty()) {
                for (range, key) in
                    super::renderers::text_matches(&value[run.clone()], self.message)
                {
                    drawn.push((run.start + range.start..run.start + range.end, key));
                }
            }
        }
        // With Emoji on, its renderer claims custom emoji first, so a
        // plugin piece running into one is not drawn (`inlineMatches`).
        if emoji_on {
            let mut emoji = Vec::new();
            let mut j = 0;
            while j < value.len() {
                match self
                    .custom_emoji(&value[j..])
                    .filter(|_| value[j..].starts_with(':'))
                {
                    Some((_, length)) => {
                        emoji.push(j..j + length);
                        j += length;
                    }
                    None => j += value[j..].chars().next().map_or(1, char::len_utf8),
                }
            }
            drawn.retain(|(range, _)| {
                !emoji
                    .iter()
                    .any(|span| span.start < range.end && range.start < span.end)
            });
        }
        let mut drawn = drawn.into_iter().peekable();
        let mut i = 0;
        while i < value.len() {
            let rest = &value[i..];
            if let Some((end, keys)) = mention_at(value, i, &self.names, spans_seam) {
                // A legacy namesake qualifier names someone else.
                let qualified = value[end..].strip_prefix(" (").is_some_and(|q| {
                    q.len() >= 65
                        && q.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
                        && q.as_bytes()[64] == b')'
                });
                if let [key] = keys
                    && !qualified
                {
                    link_md(
                        &value[i..end],
                        &format!("buzz-mention:{}", key.to_hex()),
                        out,
                    );
                } else {
                    escape(&value[i..end], out);
                }
                i = end;
                continue;
            }
            if emoji_on
                && rest.starts_with(':')
                && let Some((index, length)) = self.custom_emoji(rest)
            {
                image_md(&rest[..length], &format!("buzz-emoji:{index}"), out);
                i += length;
                continue;
            }
            if let Some(found) = link_part(value, i) {
                match &found.label {
                    Some(label) => link_md(label, &found.url, out),
                    None => link_md(&found.url, &found.url, out),
                }
                i = found.end;
                continue;
            }
            if let Some((label, id)) = self.channel_at(value, i) {
                let url = format!("buzz://channel/{}", url_encode(id));
                link_md(label, &url, out);
                i += label.len();
                continue;
            }
            // Plugins' text renderers, after mentions, emoji, links and
            // channel names.
            while drawn.next_if(|(range, _)| range.start < i).is_some() {}
            if let Some((range, key)) = drawn.next_if(|(range, _)| range.start == i) {
                image_md(&value[range.clone()], &format!("buzz-inline:{key}"), out);
                i = range.end;
                continue;
            }
            let c = rest.chars().next().expect("not at the end");
            match c {
                '\r' if rest.starts_with("\r\n") => {
                    out.push_str("\\\n");
                    i += 2;
                    continue;
                }
                '\n' | '\r' => out.push_str("\\\n"),
                _ => escape_char(c, out),
            }
            i += c.len_utf8();
        }
    }

    /// `:shortcode:` naming one of the row's own emoji: its index and the
    /// match length.
    /// A channel name at `i` (`#name` after a space or an opening
    /// bracket, ending at a space or punctuation): its label and id.
    fn channel_at<'a>(&'a self, value: &str, i: usize) -> Option<(&'a String, &'a String)> {
        let rest = &value[i..];
        if !rest.starts_with('#')
            || !(i == 0 || value[..i].ends_with(|c: char| c.is_whitespace() || "([{".contains(c)))
        {
            return None;
        }
        self.channels
            .iter()
            .find(|(label, _)| {
                rest.starts_with(label.as_str())
                    && rest[label.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| c.is_whitespace() || ".,!?;:)]}".contains(c))
            })
            .map(|(label, id)| (label, id))
    }

    fn custom_emoji(&self, rest: &str) -> Option<(usize, usize)> {
        let (code, length) = shortcode_at(rest)?;
        let index = self.emoji.iter().position(|e| e.shortcode == code)?;
        Some((index, length))
    }
}

/// A `:shortcode:` opening `rest`: the code, lowercased, and the match
/// length.
fn shortcode_at(rest: &str) -> Option<(String, usize)> {
    let body = rest.strip_prefix(':')?;
    let end = body.find(':')?;
    let code = &body[..end];
    let valid = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    valid.then(|| (code.to_ascii_lowercase(), end + 2))
}

fn url_encode(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// A link found in prose by `messageLinkParts`.
#[derive(Debug, PartialEq)]
struct LinkPart {
    label: Option<String>,
    url: String,
    end: usize,
}

fn valid_link(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.scheme() == "buzz" => {
            buzz_relay_client::parse_buzz_link(url).is_some()
                || buzz_relay_client::projects::EntityRoute::parse_link(url).is_some()
        }
        Ok(parsed) => {
            matches!(parsed.scheme(), "http" | "https")
                && parsed.username().is_empty()
                && parsed.password().is_none()
        }
        Err(_) => false,
    }
}

/// `trimBareUrl`: trailing punctuation and unbalanced closers leave.
fn trim_bare_url(candidate: &str) -> &str {
    let count = |c: char| candidate.chars().filter(|x| *x == c).count();
    let mut balance = [
        (')', count('('), count(')')),
        (']', count('['), count(']')),
        ('}', count('{'), count('}')),
    ];
    let mut end = candidate.len();
    while end > 0 {
        let c = candidate[..end].chars().next_back().expect("not empty");
        if ".,;:!?".contains(c) {
            end -= c.len_utf8();
            continue;
        }
        match balance.iter_mut().find(|(closer, _, _)| *closer == c) {
            Some((_, open, close)) if *close > *open => {
                *close -= 1;
                end -= c.len_utf8();
            }
            _ => break,
        }
    }
    &candidate[..end]
}

fn scheme_at(rest: &str) -> bool {
    let lower: String = rest
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("buzz://")
}

/// `messageLinkParts` at `i`: a `[label](url)` the parser left as text, a
/// `<url>` or a bare `http(s)://` or `buzz://` URL.
fn link_part(value: &str, i: usize) -> Option<LinkPart> {
    let rest = &value[i..];
    let url_end = |s: &str| {
        s.find(|c: char| c.is_whitespace() || "<>\"`".contains(c))
            .unwrap_or(s.len())
    };
    if let Some(inner) = rest.strip_prefix('<')
        && scheme_at(inner)
    {
        let end = url_end(inner);
        if inner[end..].starts_with('>') {
            let url = &inner[..end];
            return valid_link(url).then(|| LinkPart {
                label: None,
                url: url.to_string(),
                end: i + end + 2,
            });
        }
    }
    if scheme_at(rest) {
        let candidate = &rest[..url_end(rest)];
        let url = trim_bare_url(candidate);
        return valid_link(url).then(|| LinkPart {
            label: None,
            url: url.to_string(),
            end: i + url.len(),
        });
    }
    if rest.starts_with('[') && !value[..i].ends_with('\\') {
        let close = rest.find(']')?;
        let label = rest[1..close].trim();
        if label.is_empty() || label.contains(['\n', '\r', '[']) {
            return None;
        }
        let after = &rest[close + 1..];
        let (open_len, escaped) = if after.starts_with("\\(") {
            (2, true)
        } else if after.starts_with('(') {
            (1, false)
        } else {
            return None;
        };
        let start = close + 1 + open_len;
        let (close_at, end) = closing_parenthesis(rest, start, escaped)?;
        let mut url = markdown_destination(&rest[start..close_at]).to_string();
        // A pasted autolink can itself be Markdown: [label]([url](url)).
        if let Some(nested) = url.strip_prefix('[')
            && let Some((a, b)) = nested.split_once("](")
            && let Some(b) = b.strip_suffix(')')
            && a == b
        {
            url = a.to_string();
        }
        if url.starts_with('<') && url.ends_with('>') && url.len() >= 2 {
            url = url[1..url.len() - 1].to_string();
        }
        let url = unescape_link(&url);
        if url.contains(char::is_whitespace) || !valid_link(&url) {
            return None;
        }
        return Some(LinkPart {
            label: Some(unescape_link(label)),
            url,
            end: i + end,
        });
    }
    None
}

fn unescape_link(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
            && "[]()\\".contains(next)
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

fn markdown_destination(value: &str) -> &str {
    let trimmed = value.trim_end();
    for (open, close) in [('"', '"'), ('\'', '\''), ('(', ')')] {
        if trimmed.ends_with(close)
            && let Some(at) = trimmed[..trimmed.len() - 1].rfind(open)
            && trimmed[..at].ends_with([' ', '\t'])
        {
            return trimmed[..at].trim();
        }
    }
    value.trim()
}

/// The wrapper's end without cutting parentheses out of a URL: `(close,
/// end)` byte offsets in `content`.
fn closing_parenthesis(content: &str, start: usize, escaped: bool) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let (mut depth, mut delimiters, mut index) = (1, 0, start);
    while index < bytes.len() {
        let c = bytes[index];
        if matches!(c, b'(' | b')' | b'\\') {
            delimiters += 1;
            if delimiters > 100 {
                return None;
            }
        }
        match c {
            b'\n' | b'\r' => return None,
            b'\\' => {
                if escaped && bytes.get(index + 1) == Some(&b')') && depth == 1 {
                    return Some((index, index + 2));
                }
                index += 2;
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((index, index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Every `[label](url)` a match reports, in order: `(start, end, label,
/// url)`.
fn labeled_links(content: &str) -> Vec<(usize, usize, String, String)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < content.len() {
        let rest = &content[i..];
        if rest.starts_with('[')
            && let Some(part) = link_part(content, i)
            && part.label.is_some()
        {
            found.push((
                i,
                part.end,
                part.label.clone().unwrap_or_default(),
                part.url,
            ));
            i = part.end;
            continue;
        }
        if (scheme_at(rest) || (rest.starts_with('<') && scheme_at(&rest[1..])))
            && let Some(part) = link_part(content, i)
        {
            i = part.end.max(i + 1);
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// `normalizeWrappedLinks`: pasted `[label]\(url\)` and `[label]([url](url))`
/// become ordinary links, except in literal contexts.
pub fn normalize_wrapped_links(content: &str, literal: impl Fn(usize, usize) -> bool) -> String {
    if !content.contains("]\\(") && !content.contains("](") {
        return content.to_string();
    }
    let mut result = String::new();
    let mut offset = 0;
    for (start, end, label, url) in labeled_links(content) {
        let source = &content[start..end];
        if literal(start, end) || (!source.contains("]\\(") && !source.contains("]([")) {
            continue;
        }
        result.push_str(&content[offset..start]);
        result.push('[');
        for c in label.chars() {
            if matches!(c, '\\' | '[' | ']') {
                result.push('\\');
            }
            result.push(c);
        }
        result.push_str("](");
        result.push_str(&url.replace('(', "%28").replace(')', "%29"));
        result.push(')');
        offset = end;
    }
    result.push_str(&content[offset..]);
    result
}

/// The visible text of inline nodes, as a link label.
fn plain_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(&text.value),
            Node::InlineCode(code) => out.push_str(&code.value),
            Node::Image(image) => out.push_str(&image.alt),
            Node::Break(_) => out.push(' '),
            Node::Html(html) => {
                out.push_str(&html_lines(&visible_html_source(&html.value)).join(" "))
            }
            other => out.push_str(&plain_text(other.children().map_or(&[][..], Vec::as_slice))),
        }
    }
    out
}

/// The visible text of one-paragraph Markdown written here.
fn plain_markdown(source: &str) -> String {
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(tree) => plain_text(tree.children().map_or(&[][..], Vec::as_slice)),
        Err(_) => source.to_string(),
    }
}

fn escape_char(c: char, out: &mut String) {
    if c.is_ascii_punctuation() {
        out.push('\\');
    }
    out.push(c);
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\n' | '\r' => out.push(' '),
            c => escape_char(c, out),
        }
    }
}

/// Chat never renders raw HTML, but `<harness>` is still something the
/// author typed: its exact source as text, with chat line breaks, and no
/// mention, emoji or link processing. Complete HTML comments stay hidden.
fn literal_html(value: &str, out: &mut String) {
    let source = visible_html_source(value);
    for (index, line) in html_lines(&source).iter().enumerate() {
        if index > 0 {
            out.push_str("\\\n");
        }
        escape(line, out);
    }
}

/// Lines split at `\r\n`, `\n` or `\r`.
fn html_lines(source: &str) -> Vec<String> {
    source
        .replace("\r\n", "\n")
        .split(['\n', '\r'])
        .map(str::to_owned)
        .collect()
}

/// `<…>` destinations allow any character but these.
fn destination(url: &str) -> String {
    url.replace('<', "%3C")
        .replace('>', "%3E")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
}

fn link_md(label: &str, url: &str, out: &mut String) {
    out.push('[');
    escape(label, out);
    out.push_str("](<");
    out.push_str(&destination(url));
    out.push_str(">)");
}

fn image_md(alt: &str, url: &str, out: &mut String) {
    out.push_str("![");
    escape(alt, out);
    out.push_str("](<");
    out.push_str(&destination(url));
    out.push_str(">)");
}

fn code_span(value: &str, out: &mut String) {
    let mut longest = 0;
    let mut run = 0;
    for c in value.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = value.starts_with('`') || value.ends_with('`') || value.starts_with(' ');
    out.push_str(&fence);
    if pad {
        out.push(' ');
    }
    out.push_str(value);
    if pad {
        out.push(' ');
    }
    out.push_str(&fence);
}

// ---------------------------------------------------------------------------
// Emoji-only bodies (`emoji-size.ts`)

/// Unicode pictographs, flags and keycaps, plus the row's own
/// `:shortcodes:`, with whitespace between them and nothing else.
pub(crate) fn is_emoji_only(content: &str, emoji: &[CustomEmoji]) -> bool {
    emoji_only(&emoji_parts(content.trim(), emoji))
}

/// [`is_emoji_only`] against any catalog, by whether it `knows` a
/// lowercase shortcode.
pub(crate) fn is_emoji_only_by(content: &str, knows: impl Fn(&str) -> bool) -> bool {
    emoji_only(&split_emoji(content.trim(), |code| {
        knows(code).then_some(())
    }))
}

fn emoji_only<E>(parts: &[EmojiPart<E>]) -> bool {
    if parts.is_empty() {
        return false;
    }
    let mut any = false;
    for part in parts {
        match part {
            EmojiPart::Custom(_) => any = true,
            EmojiPart::Text(text) => {
                let chars: Vec<char> = text.chars().collect();
                for (index, &c) in chars.iter().enumerate() {
                    if c.is_whitespace() || is_emoji_modifier(c) {
                        continue;
                    }
                    if is_pictographic(c) {
                        any = true;
                        continue;
                    }
                    // Keycaps: [0-9#*] FE0F? 20E3.
                    let keycap = (c.is_ascii_digit() || matches!(c, '#' | '*'))
                        && match chars.get(index + 1) {
                            Some('\u{20E3}') => true,
                            Some('\u{FE0F}') => chars.get(index + 2) == Some(&'\u{20E3}'),
                            _ => false,
                        };
                    if keycap {
                        any = true;
                        continue;
                    }
                    return false;
                }
            }
        }
    }
    any
}

/// Characters that only modify an emoji sequence.
fn is_emoji_modifier(c: char) -> bool {
    matches!(c as u32,
        0x200D | 0xFE0E | 0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F)
}

/// `\p{Extended_Pictographic}` and regional indicators, by the ranges that
/// hold them.
fn is_pictographic(c: char) -> bool {
    matches!(c as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x2199 | 0x21A9..=0x21AA
        | 0x231A..=0x231B | 0x2328 | 0x2388 | 0x23CF | 0x23E9..=0x23F3 | 0x23F8..=0x23FA
        | 0x24C2 | 0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FE | 0x2600..=0x27BF
        | 0x2934..=0x2935 | 0x2B05..=0x2B07 | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x3030
        | 0x303D | 0x3297 | 0x3299 | 0x1F000..=0x1FAFF | 0x1FC00..=0x1FFFD)
}

fn emoji_parts(content: &str, emoji: &[CustomEmoji]) -> Vec<EmojiPart> {
    split_emoji(content, |code| {
        emoji.iter().find(|entry| entry.shortcode == code).cloned()
    })
}

/// Text runs and the `:shortcodes:` `find` resolves.
fn split_emoji<E>(content: &str, find: impl Fn(&str) -> Option<E>) -> Vec<EmojiPart<E>> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < content.len() {
        let rest = &content[i..];
        if let Some((code, length)) = shortcode_at(rest)
            && let Some(entry) = find(&code)
        {
            if !text.is_empty() {
                parts.push(EmojiPart::Text(std::mem::take(&mut text)));
            }
            parts.push(EmojiPart::Custom(entry));
            i += length;
            continue;
        }
        let c = rest.chars().next().expect("not at the end");
        text.push(c);
        i += c.len_utf8();
    }
    if !text.is_empty() {
        parts.push(EmojiPart::Text(text));
    }
    parts
}

fn jumbo(parts: &[EmojiPart], id: &str, media: &Media) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .mt(px(4.))
        .min_h(px(48.))
        .text_size(px(42.))
        // Every wrapped line takes the emoji line height.
        .line_height(px(48.))
        .children(parts.iter().enumerate().map(|(index, part)| {
            match part {
                EmojiPart::Text(text) => SelectableText::new(
                    SharedString::from(format!("jumbo-{id}-{index}")),
                    SharedString::from(text.clone()),
                )
                .into_any_element(),
                EmojiPart::Custom(entry) => {
                    custom_emoji(entry, 42., &format!("jumbo-{id}-{index}"), media)
                        .into_any_element()
                }
            }
        }))
        .into_any_element()
}

/// A custom emoji with its `:shortcode:` as tooltip and fallback.
fn custom_emoji(entry: &CustomEmoji, size: f32, id: &str, media: &Media) -> impl IntoElement {
    let code = format!(":{}:", entry.shortcode);
    let fallback = code.clone();
    div()
        .id(SharedString::from(format!("emoji-{id}")))
        .flex_none()
        .tooltip(Tooltip::text(code))
        .child(
            img(media::image(&entry.url, media))
                .size(px(size))
                .object_fit(ObjectFit::Contain)
                .with_fallback(move || {
                    div()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .child(fallback.clone())
                        .into_any_element()
                }),
        )
}

// ---------------------------------------------------------------------------
// Layout

struct Out<'a> {
    objects: Objects,
    cx: &'a App,
    /// Inside a quote, text is subtle.
    quote: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Container {
    /// The body: first and last margins drop (`.text > :first-child`).
    Root,
    /// Quotes keep their children's margins inside their padding.
    Quote,
    /// List items space their blocks 4px apart.
    Item,
}

/// CSS block margins (top, bottom), which collapse between siblings.
fn margins(block: &Block) -> (f32, f32) {
    match block {
        Block::Heading(..) => (16., 6.),
        _ => (16., 16.),
    }
}

fn column(blocks: &[Block], container: Container, path: &str, out: &mut Out) -> Div {
    let mut column = div().flex().flex_col().w_full().min_w_0();
    let mut previous: Option<f32> = None;
    for (index, block) in blocks.iter().enumerate() {
        let (top, bottom) = match container {
            Container::Item => (4., 4.),
            _ => margins(block),
        };
        let gap = match previous {
            Some(previous) => previous.max(top),
            None if container == Container::Quote => top,
            None => 0.,
        };
        let element = render_block(block, &format!("{path}-{index}"), out);
        column = column.child(div().pt(px(gap)).child(element));
        previous = Some(bottom);
    }
    if container == Container::Quote
        && let Some(bottom) = previous
    {
        column = column.pb(px(bottom));
    }
    column
}

fn list(
    ordered: bool,
    start: u32,
    items: &[Item],
    path: &str,
    text: Hsla,
    out: &mut Out,
) -> AnyElement {
    let c = *out.cx.colors();
    let mut list = div().flex().flex_col().w_full();
    for (index, item) in items.iter().enumerate() {
        let marker = match (item.checked, ordered) {
            (Some(checked), _) => checkbox(checked, &c).into_any_element(),
            (None, true) => div()
                .pr(px(4.))
                .child(format!("{}.", start as usize + index))
                .into_any_element(),
            (None, false) => div().pr(px(10.)).child("•").into_any_element(),
        };
        let content = column(
            &item.blocks,
            Container::Item,
            &format!("{path}-{index}"),
            out,
        );
        list = list.child(
            div()
                .flex()
                .when(index > 0, |d| d.mt(px(4.)))
                .child(
                    div()
                        .flex_none()
                        .w(px(24.))
                        .flex()
                        .justify_end()
                        .text_color(text)
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(content)),
        );
    }
    list.into_any_element()
}

fn render_block(block: &Block, path: &str, out: &mut Out) -> AnyElement {
    let c = *out.cx.colors();
    let text = if out.quote {
        c.text_subtle
    } else {
        body_text(&c)
    };
    match block {
        Block::Paragraph(source) => inline(source, path, text, true, out),
        Block::Heading(depth, source) => {
            let (size, line) = match depth {
                1 => (24., 26.4),
                2 => (20., 28.),
                3 => (16., 24.),
                _ => (14., 20.),
            };
            div()
                .text_size(px(size))
                .line_height(px(line))
                .font_weight(FontWeight::MEDIUM)
                .child(inline(source, path, text, true, out))
                .into_any_element()
        }
        Block::Quote(blocks) => {
            let quoted = std::mem::replace(&mut out.quote, true);
            let inner = column(blocks, Container::Quote, path, out);
            out.quote = quoted;
            div()
                .py(px(2.))
                .pl(px(12.))
                .border_l_1()
                .border_color(c.border_prominent)
                .text_color(c.text_subtle)
                .child(inner)
                .into_any_element()
        }
        Block::List {
            ordered,
            start,
            items,
        } => list(*ordered, *start, items, path, text, out),
        Block::Footnotes(notes) => {
            let items: Vec<Item> = Vec::new();
            let _ = items;
            let mut list = div().flex().flex_col().w_full().text_color(text);
            for (index, note) in notes.iter().enumerate() {
                let content = column(note, Container::Item, &format!("{path}-n{index}"), out);
                list = list.child(
                    div()
                        .flex()
                        .when(index > 0, |d| d.mt(px(4.)))
                        .child(
                            div()
                                .flex_none()
                                .w(px(24.))
                                .flex()
                                .justify_end()
                                .pr(px(4.))
                                .child(format!("{}.", index + 1)),
                        )
                        .child(div().flex_1().min_w_0().child(content)),
                );
            }
            list.into_any_element()
        }
        Block::Code(code) => div()
            .id(SharedString::from(format!("code{path}")))
            .w_full()
            .p(px(12.))
            .border_1()
            .border_color(c.border_standard)
            .rounded(px(8.))
            .bg(c.surface_panel)
            .overflow_x_scroll()
            .font_family(font::MONO)
            .text_size(px(12.))
            .line_height(px(16.))
            .text_color(text)
            .child(div().whitespace_nowrap().child(SelectableText::new(
                SharedString::from(format!("code-text{path}-{}", out.objects.row)),
                SharedString::from(code.trim_end_matches('\n').to_string()),
            )))
            .into_any_element(),
        Block::Table { align, rows } => table(rows, align, path, text, out),
        Block::Rule => div()
            .h(px(1.))
            .w_full()
            .bg(c.border_standard)
            .into_any_element(),
    }
}

fn checkbox(checked: bool, c: &buzz_ui::Colors) -> impl IntoElement {
    div()
        .mr(px(6.))
        .mt(px(3.))
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .border_color(if checked {
            c.affordance_prominent
        } else {
            c.border_prominent
        })
        .when(checked, |d| {
            d.bg(c.affordance_prominent)
                .child(Icon::new(IconName::Check).size(10.).color(c.text_inverse))
        })
}

/// Cells are a column per table column, so columns share a width without
/// a grid; cells stay on one line (`nowrap`), so rows line up. Each cell
/// is a selectable `TextView` with the same links, mentions and emoji as
/// prose.
fn table(
    rows: &[Vec<SharedString>],
    align: &[AlignKind],
    path: &str,
    text: Hsla,
    out: &mut Out,
) -> AnyElement {
    let c = *out.cx.colors();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut frame = div()
        .flex()
        .border_1()
        .border_color(c.border_standard)
        .rounded(px(12.))
        .overflow_hidden()
        .text_color(text);
    for column in 0..columns {
        let mut cells = div().flex().flex_col().flex_none();
        for (index, row) in rows.iter().enumerate() {
            let head = index == 0;
            let source = row.get(column).cloned().unwrap_or_default();
            let cell = div()
                .h(px(28.))
                .px(px(8.))
                .py(px(4.))
                .whitespace_nowrap()
                .flex()
                .when(column + 1 < columns, |d| {
                    d.border_r_1().border_color(c.border_standard)
                })
                .when(index + 1 < rows.len(), |d| {
                    d.border_b_1().border_color(c.border_standard)
                })
                .when(head, |d| {
                    d.bg(c.affordance_subtle).font_weight(FontWeight::MEDIUM)
                })
                .map(|d| match align.get(column) {
                    Some(AlignKind::Center) => d.justify_center(),
                    Some(AlignKind::Right) => d.justify_end(),
                    _ => d,
                })
                .when(!source.is_empty(), |d| {
                    d.child(inline(
                        &source,
                        &format!("{path}-{index}-{column}"),
                        text,
                        false,
                        out,
                    ))
                });
            cells = cells.child(cell);
        }
        frame = frame.child(cells);
    }
    div()
        .id(SharedString::from(format!("table{path}")))
        .max_w_full()
        .overflow_x_scroll()
        .child(div().flex().child(frame))
        .into_any_element()
}

fn text_style(color: Hsla, c: &buzz_ui::Colors) -> TextViewStyle {
    TextViewStyle::default()
        .with_foreground(color)
        .with_muted_foreground(c.text_subtle)
        .with_link(c.text_link)
        .with_border(c.border_standard)
        .with_code_background(c.affordance_subtle)
        .with_inline_code(HighlightStyle {
            background_color: Some(c.affordance_subtle),
            ..Default::default()
        })
        .with_selection(c.text_link.opacity(0.25))
        .with_paragraph_gap(rems(0.))
}

/// A paragraph's inline content in a selectable `TextView`. `wrap` is off
/// for table cells, which keep their natural width.
fn inline(source: &SharedString, path: &str, color: Hsla, wrap: bool, out: &Out) -> AnyElement {
    let c = out.cx.colors();
    let extensions = MarkdownExtensions::default()
        .parser_revision(PARSER_REVISION)
        .plugin(out.objects.clone());
    let view = TextView::markdown(
        SharedString::from(format!("t{path}-{}", out.objects.row)),
        source.clone(),
    )
    .style(text_style(color, c))
    .markdown_extensions(extensions);
    if wrap {
        view.w_full().into_any_element()
    } else {
        div().whitespace_nowrap().child(view).into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Inline objects

/// Bumped when [`Objects::parse`] changes.
const PARSER_REVISION: u64 = 0xb022_0002;
/// Longer code spans stay in the text flow so they can wrap; shorter ones
/// get the padded chip.
const CODE_CHIP_MAX: usize = 40;

#[derive(Clone)]
struct Objects {
    directory: Directory,
    emoji: Arc<[CustomEmoji]>,
    row: SharedString,
    /// The message a mention in it opens a profile from.
    /// The message its links come from; `None` for text that is no
    /// message, such as a plugin's `markdown`.
    origin: Option<navigation::LinkOrigin>,
    spoilers: Arc<[SharedString]>,
    /// The body's width, for chips and spoilers that must wrap inside it.
    max_width: Option<Pixels>,
    /// How its links open, and whether renderers are left out: chips are
    /// made in layout, after the body was built.
    link_mode: links::LinkMode,
    plain: bool,
}

enum Object {
    Link { url: String, label: StyledLabel },
    Image { url: String, alt: String },
    Code(String),
}

/// A label's text and styles, from its parsed children.
fn styled_label(nodes: &[Node], c: &buzz_ui::Colors) -> StyledLabel {
    fn walk(nodes: &[Node], style: HighlightStyle, out: &mut StyledLabel, c: &buzz_ui::Colors) {
        for node in nodes {
            let mut style = style;
            let text = match node {
                Node::Text(text) => text.value.clone(),
                Node::InlineCode(code) => {
                    style.background_color = Some(c.affordance_subtle);
                    style.font_weight = style.font_weight.or(None);
                    code.value.clone()
                }
                Node::Break(_) => " ".into(),
                Node::Image(image) => image.alt.clone(),
                Node::Html(html) => html_lines(&visible_html_source(&html.value)).join(" "),
                _ => {
                    match node {
                        Node::Strong(_) => style.font_weight = Some(FontWeight::MEDIUM),
                        Node::Emphasis(_) => style.font_style = Some(FontStyle::Italic),
                        Node::Delete(_) => {
                            style.strikethrough = Some(StrikethroughStyle {
                                thickness: px(1.),
                                color: None,
                            })
                        }
                        _ => {}
                    }
                    walk(
                        node.children().map_or(&[][..], Vec::as_slice),
                        style,
                        out,
                        c,
                    );
                    continue;
                }
            };
            if text.is_empty() {
                continue;
            }
            let start = out.text.len();
            out.text.push_str(&text);
            if style != HighlightStyle::default() {
                out.runs.push((start..out.text.len(), style));
            }
        }
    }
    let mut out = StyledLabel::default();
    walk(nodes, HighlightStyle::default(), &mut out, c);
    out
}

impl MarkdownPlugin for Objects {
    fn name(&self) -> &str {
        "buzz"
    }

    fn parse(&self, node: &mdast::Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let colors = buzz_ui::Colors::light();
        let (object, text) = match node {
            Node::Link(link) => {
                let label = styled_label(&link.children, &colors);
                let copy = copy_text(&link.url, &label.text, &self.directory);
                (
                    Object::Link {
                        url: link.url.clone(),
                        label,
                    },
                    copy,
                )
            }
            Node::Image(image) => {
                let text = match image.url.strip_prefix("buzz-emoji:") {
                    // Copied as its `:shortcode:` (`copy-emoji.ts`).
                    Some(index) => index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| self.emoji.get(index))
                        .map_or_else(|| image.alt.clone(), |e| format!(":{}:", e.shortcode)),
                    None if image.url.starts_with("buzz-spoiler:")
                        || image.url.starts_with("buzz-footnote:") =>
                    {
                        image.alt.clone()
                    }
                    None => image.alt.clone(),
                };
                (
                    Object::Image {
                        url: image.url.clone(),
                        alt: image.alt.clone(),
                    },
                    text,
                )
            }
            Node::InlineCode(code) if code.value.chars().count() <= CODE_CHIP_MAX => {
                (Object::Code(code.value.clone()), code.value.clone())
            }
            _ => return None,
        };
        Some(MarkdownNode::new("buzz", object).text(text))
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let c = *cx.colors();
        let size = context.font_size();
        let line = context.line_height();
        // Inter's ascent and descent are 0.969 and 0.241 em.
        let baseline = (line - size * 1.21) / 2. + size * 0.969;
        match node.data::<Object>()? {
            Object::Code(code) => {
                // JetBrains Mono 12/16 with 2px padding; its ascent is 1.02 em.
                let chip_baseline = px(2. + (16. - 15.84) / 2. + 12.24);
                Some(
                    InlineElement::new(
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(c.affordance_subtle)
                            .font_family(font::MONO)
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .whitespace_nowrap()
                            .child(code.clone()),
                    )
                    .with_baseline(chip_baseline),
                )
            }
            Object::Image { url, alt } => {
                if let Some(index) = url.strip_prefix("buzz-emoji:") {
                    let entry = self.emoji.get(index.parse::<usize>().ok()?)?;
                    // 22px, `vertical-align: middle`: centred on half the
                    // x-height (0.546 em) above the baseline.
                    let id = format!(
                        "{}-{index}-{}",
                        self.row,
                        node.source_range().map_or(0, |r| r.start)
                    );
                    return Some(
                        InlineElement::new(custom_emoji(entry, 22., &id, &self.directory.media))
                            .with_baseline(px(11.) + size * 0.273),
                    );
                }
                if let Some(index) = url.strip_prefix("buzz-spoiler:") {
                    let index = index.parse::<usize>().ok()?;
                    let source = self.spoilers.get(index)?.clone();
                    return Some(
                        InlineElement::new(
                            self.spoiler(index, source, alt, size, line, window, cx),
                        )
                        .with_baseline(baseline),
                    );
                }
                if let Some(key) = url.strip_prefix("buzz-inline:") {
                    let element = super::renderers::render_text(key, alt, &self.row, cx)
                        .unwrap_or_else(|| {
                            div()
                                .line_height(line)
                                .child(alt.clone())
                                .into_any_element()
                        });
                    return Some(InlineElement::new(element).with_baseline(baseline));
                }
                if let Some(number) = url.strip_prefix("buzz-footnote:") {
                    // `<sup><a>1</a></sup>`: a small raised, underlined mark.
                    return Some(
                        InlineElement::new(
                            div()
                                .text_size(size * 0.75)
                                .line_height(size)
                                .text_color(c.text_standard)
                                .underline()
                                .child(number.to_string()),
                        )
                        .with_baseline(size * 0.75),
                    );
                }
                // Markdown images are never loaded: the alt text stands in.
                if alt.is_empty() {
                    return None;
                }
                Some(
                    InlineElement::new(
                        div()
                            .italic()
                            .text_color(c.text_subtle)
                            .line_height(line)
                            .child(alt.clone()),
                    )
                    .with_baseline(baseline),
                )
            }
            Object::Link { url, label } => {
                if url.starts_with("buzz-footnote-back:") {
                    return Some(
                        InlineElement::new(
                            div()
                                .line_height(line)
                                .text_color(c.text_standard)
                                .underline()
                                .child("↩"),
                        )
                        .with_baseline(baseline),
                    );
                }
                let id = SharedString::from(format!(
                    "{}-{}",
                    self.row,
                    node.source_range().map_or(0, |r| r.start)
                ));
                let element = links::with_mode(self.link_mode, || {
                    super::renderers::with_plain(self.plain, || {
                        self.link(url, label, id, size, line, window, cx)
                    })
                })?;
                Some(InlineElement::new(element).with_baseline(baseline))
            }
        }
    }
}

impl Objects {
    #[allow(clippy::too_many_arguments)]
    fn link(
        &self,
        url: &str,
        label: &StyledLabel,
        id: SharedString,
        size: Pixels,
        line: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let c = *cx.colors();
        let mention = url
            .strip_prefix("buzz-mention:")
            .map(str::to_string)
            .or_else(|| buzz_relay_client::profile_key(url).map(|key| key.to_hex()));
        if let Some(key) = mention {
            let names = &self.directory.names;
            let fallback = label.text.strip_prefix('@').unwrap_or(&label.text);
            let name = names.name(&key, fallback);
            let agent = names.is_agent(&key);
            let (icon, gap) = if agent {
                (IconName::Robot, 3.)
            } else {
                (IconName::At, 0.)
            };
            let tooltip = format!("{}: {name}\n{key}", if agent { "Agent" } else { "Person" });
            let origin = self.origin.clone();
            return Some(
                div()
                    .id(SharedString::from(format!("mention-{id}-{key}")))
                    .flex()
                    .items_center()
                    .gap(px(gap))
                    .px(px(2.))
                    .mx(px(-2.))
                    .rounded(px(4.))
                    .line_height(line)
                    .whitespace_nowrap()
                    .text_color(c.text_link)
                    .tooltip(Tooltip::text(tooltip))
                    .child(Icon::new(icon).size(f32::from(size)).color(c.text_link))
                    .child(name)
                    // A reader opens no profiles (`canOpenLink` unset).
                    .when(self.link_mode != links::LinkMode::Closed, |d| {
                        d.hover(|style| style.bg(c.affordance_link_hover))
                            .cursor_pointer()
                            .on_click(move |event, window, cx| {
                                cx.stop_propagation();
                                let origin = origin.clone();
                                navigation::show_profile_from(
                                    key.clone(),
                                    event.position(),
                                    origin,
                                    window,
                                    cx,
                                )
                            })
                    })
                    .into_any_element(),
            );
        }
        let authored = (!label.text.is_empty() && label.text != url).then(|| label.clone());
        Some(links::chip(
            Chip {
                id,
                url,
                label: authored,
                directory: &self.directory,
                font_size: size,
                line_height: line,
                max_width: self.max_width,
                origin: self.origin.clone(),
            },
            window,
            cx,
        ))
    }

    /// `||text||`: dots over hidden text until clicked; a click toggles it.
    /// The content keeps its formatting and links, and wraps inside the
    /// body like an inline block.
    #[allow(clippy::too_many_arguments)]
    fn spoiler(
        &self,
        index: usize,
        source: SharedString,
        plain: &str,
        size: Pixels,
        line: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let c = *cx.colors();
        let id = SharedString::from(format!("spoiler-{}-{index}", self.row));
        let revealed = window.use_keyed_state(id.clone(), cx, |_, _| false);
        let shown = *revealed.read(cx);
        // Wide enough for the text, but never wider than the body: then it
        // wraps inside, as an inline block does.
        let natural = window
            .text_system()
            .shape_line(
                SharedString::from(plain.to_string()),
                size,
                &[window.text_style().to_run(plain.len())],
                None,
            )
            .width
            + px(6.);
        let width = match self.max_width {
            Some(max) if natural > max => Some(max),
            _ => None,
        };
        let style = text_style(c.text_standard, &c);
        let inner = TextView::markdown(SharedString::from(format!("{id}-text")), source)
            .style(style)
            .markdown_extensions(
                MarkdownExtensions::default()
                    .parser_revision(PARSER_REVISION)
                    .plugin(self.clone()),
            );
        let group = SharedString::from(format!("{id}-group"));
        div()
            .id(id)
            .group(group.clone())
            .relative()
            .px(px(2.))
            .rounded(px(4.))
            .line_height(line)
            .when_some(width, |d, width| d.w(width))
            .cursor_pointer()
            .when(shown, |d| d.bg(c.affordance_subtle))
            .when(!shown, |d| d.hover(|s| s.bg(c.affordance_subtle)))
            .child(div().when(!shown, |d| d.invisible()).child(inner))
            .when(!shown, |d| d.child(spoiler_mask(c.text_standard)))
            .on_click(move |_, _, cx| {
                revealed.update(cx, |shown, cx| {
                    *shown = !*shown;
                    cx.notify();
                })
            })
            .into_any_element()
    }
}

/// The spoiler's mask: two fields of tiled dots twinkling between 35% and
/// 90% and drifting a pixel, out of step (`.spoilerMask::before/after`).
fn spoiler_mask(color: Hsla) -> impl IntoElement {
    // (tile width, tile height, x, y, radius) per dot pattern.
    const DOTS: [(f32, f32, f32, f32, f32); 3] = [
        (11., 13., 2., 3., 0.8),
        (17., 11., 8., 7., 0.7),
        (13., 17., 5., 11., 0.85),
    ];
    let field = |shift: (f32, f32), period: u64, reverse: bool, name: &'static str| {
        div().absolute().inset_0().overflow_hidden().with_animation(
            name,
            Animation::new(Duration::from_millis(period))
                .repeat()
                .with_easing(ease_in_out),
            move |d, delta| {
                // `alternate`: there and back.
                let t = if delta < 0.5 {
                    delta * 2.
                } else {
                    2. - delta * 2.
                };
                let t = if reverse { 1. - t } else { t };
                let opacity = 0.35 + 0.55 * t;
                let drift = -1. + 2. * t;
                d.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let color = color.opacity(opacity);
                            for (tile_w, tile_h, x, y, r) in DOTS {
                                let mut ty = bounds.top() + px(y + shift.1 - drift);
                                while ty < bounds.bottom() {
                                    let mut tx = bounds.left() + px(x + shift.0 + drift);
                                    while tx < bounds.right() {
                                        window.paint_quad(
                                            fill(
                                                Bounds::new(
                                                    point(tx - px(r), ty - px(r)),
                                                    size(px(2. * r), px(2. * r)),
                                                ),
                                                color,
                                            )
                                            .corner_radii(px(r)),
                                        );
                                        tx += px(tile_w);
                                    }
                                    ty += px(tile_h);
                                }
                            }
                        },
                    )
                    .size_full(),
                )
            },
        )
    };
    div()
        .absolute()
        .inset_0()
        .rounded(px(4.))
        .overflow_hidden()
        .child(field((0., 0.), 650, false, "spoiler-twinkle-a"))
        .child(field((5., 7.), 900, true, "spoiler-twinkle-b"))
}

/// What selecting and copying a link yields: an authored label and its
/// destination, "label (url)"; a raw destination copies as itself;
/// mentions as "@Name".
fn copy_text(url: &str, label: &str, directory: &Directory) -> String {
    if let Some(key) = url
        .strip_prefix("buzz-mention:")
        .map(str::to_string)
        .or_else(|| buzz_relay_client::profile_key(url).map(|k| k.to_hex()))
    {
        let fallback = label.strip_prefix('@').unwrap_or(label);
        return format!("@{}", directory.names.name(&key, fallback));
    }
    if url.starts_with("buzz-footnote-back:") {
        return "↩".into();
    }
    if label.is_empty() || label == url {
        url.to_string()
    } else {
        format!("{label} ({url})")
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    // gpui's `test` macro comes in with the glob above.
    use core::prelude::v1::test;

    /// Media for a directory in tests; nothing is fetched.
    pub(in crate::ui) fn media() -> Media {
        static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
            std::sync::LazyLock::new(|| {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("test runtime")
            });
        Media::new(
            RUNTIME.handle().clone(),
            buzz_relay_client::RelayOrigin::parse("ws://127.0.0.1:1").expect("origin"),
            Arc::new(buzz_identity::Identity::ephemeral()),
        )
    }

    fn directory() -> Directory {
        Directory::new(Profiles::default(), Arc::from([]), media())
    }

    fn blocks(content: &str) -> Vec<Block> {
        let row = row(content);
        match prepare(&row, &directory()).body {
            Document::Blocks(blocks) => blocks,
            other => panic!("not blocks: {other:?}"),
        }
    }

    pub(in crate::ui) fn row(content: &str) -> MessageRow {
        MessageRow {
            id: nostr::prelude::EventId::from_byte_array([1; 32]),
            channel: Arc::from("c"),
            kind: 9,
            audience: None,
            author: buzz_identity::Identity::ephemeral().public_key(),
            signer: None,
            workflow_owner: None,
            created_at: 0,
            created_at_ms: 0,
            content: content.into(),
            source_content: None,
            edited: false,
            attachment_seams: Vec::new(),
            attachment_source_id: None,
            attachments: Vec::new(),
            emoji: Vec::new(),
            mentions: Vec::new(),
            mention_references: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            participants: Vec::new(),
            thread_root_id: None,
            reply_parent_id: None,
            sent_from_thread: None,
            diff: None,
            membership: None,
            delivery: None,
        }
    }

    fn paragraph(content: &str) -> String {
        match blocks(content).as_slice() {
            [Block::Paragraph(text)] => text.to_string(),
            other => panic!("not one paragraph: {other:?}"),
        }
    }

    #[test]
    fn binds_signed_recipients_by_exact_name() {
        let sam = buzz_identity::Identity::ephemeral().public_key();
        let profile = buzz_relay_client::Profile {
            name: "Sam Rivera".into(),
            ..buzz_relay_client::Profile::fallback(&sam.to_hex())
        };
        let profiles: Profiles = Arc::new(HashMap::from([(sam.to_hex(), Arc::new(profile))]));
        let directory = Directory::new(profiles, Arc::from([]), media());
        let mut row = row("@Sam Rivera could you `@Sam Rivera` look, @Sam Riveras?");
        row.mentions = vec![sam];
        let Document::Blocks(blocks) = prepare(&row, &directory).body else {
            panic!("not blocks");
        };
        let target = format!("[\\@Sam Rivera](<buzz-mention:{}>)", sam.to_hex());
        assert_eq!(
            blocks,
            vec![Block::Paragraph(
                format!("{target} could you `@Sam Rivera` look\\, \\@Sam Riveras\\?").into()
            )]
        );
        // Edited bodies stay unbound.
        row.edited = true;
        let Document::Blocks(blocks) = prepare(&row, &directory).body else {
            panic!("not blocks");
        };
        assert!(!format!("{blocks:?}").contains("buzz-mention"));
    }

    #[test]
    fn finds_emoji_and_spoilers_in_prose_only() {
        let mut row = row("A :Party: ||secret|| `:party:` [:party:](https://x.test)");
        row.emoji = vec![CustomEmoji {
            shortcode: "party".into(),
            url: "https://x.test/p.png".into(),
        }];
        let prepared = prepare(&row, &directory());
        assert_eq!(
            prepared.body,
            Document::Blocks(vec![Block::Paragraph(
                "A ![\\:Party\\:](<buzz-emoji:0>) ![secret](<buzz-spoiler:0>) `:party:` \
                 [\\:party\\:](<https://x.test>)"
                    .into()
            )])
        );
        assert_eq!(&*prepared.spoilers, &[SharedString::from("secret")]);
    }

    #[test]
    fn text_renderers_draw_matches_in_prose_only() {
        struct Tickets;
        impl super::super::renderers::TextRenderer for Tickets {
            fn key(&self) -> &str {
                "test.tickets/t"
            }
            fn find(
                &self,
                text: &str,
                _: super::super::renderers::InMessage,
            ) -> Vec<std::ops::Range<usize>> {
                text.match_indices("T-1")
                    .map(|(at, m)| at..at + m.len())
                    .collect()
            }
            fn render(&self, _: &str, _: &str, _: &App) -> Option<AnyElement> {
                None
            }
        }
        let before = super::super::renderers::text_generation();
        super::super::renderers::register_text(std::rc::Rc::new(Tickets));
        assert_ne!(super::super::renderers::text_generation(), before);
        let text = paragraph("See T-1, `T-1` and [T-1](https://x.test)");
        super::super::renderers::unregister("test.tickets/t");
        assert_eq!(
            text,
            "See ![T\\-1](<buzz-inline:test.tickets/t>)\\, `T-1` and [T\\-1](<https://x.test>)"
        );
    }

    #[test]
    fn spoilers_hold_formatting_across_siblings() {
        let row = row("a ||b *c* [d](https://x.test)|| e ||| f |||| g || h");
        let prepared = prepare(&row, &directory());
        let Document::Blocks(blocks) = &prepared.body else {
            panic!("not blocks");
        };
        assert_eq!(prepared.spoilers[0], "b *c* [d](<https://x.test>)");
        let text = format!("{blocks:?}");
        assert!(text.contains("buzz-spoiler:0"));
        // An escaped mark is text.
        let row = super::tests::row("a \\|\\|b\\|\\| c");
        assert!(prepare(&row, &directory()).spoilers.is_empty());
    }

    #[test]
    fn link_labels_keep_formatting() {
        assert_eq!(
            paragraph("see [**bold** and `code`](https://x.test)"),
            "see [**bold** and `code`](<https://x.test>)"
        );
    }

    #[test]
    fn bare_buzz_links_and_channel_names_become_links() {
        let id = nostr::prelude::EventId::from_byte_array([2; 32]).to_hex();
        let text = paragraph(&format!("open buzz://channel/general/{id}."));
        assert!(text.contains(&format!("](<buzz://channel/general/{id}>)\\.")));
        let channels: Arc<[ChannelSummary]> = Arc::from(vec![ChannelSummary {
            id: "c-1".into(),
            name: "design".into(),
            ..Default::default()
        }]);
        let directory = Directory::new(Profiles::default(), channels, media());
        let row = row("see #design, not #designers or a#design");
        let Document::Blocks(blocks) = prepare(&row, &directory).body else {
            panic!("not blocks");
        };
        let text = format!("{blocks:?}");
        assert_eq!(text.matches("buzz://channel/c-1").count(), 1);
    }

    #[test]
    fn repairs_pasted_wrapped_links() {
        assert_eq!(
            normalize_wrapped_links("go [here]\\(https://x.test/a\\) now", |_, _| false),
            "go [here](https://x.test/a) now"
        );
        assert_eq!(
            normalize_wrapped_links("[x]([https://x.test](https://x.test))", |_, _| false),
            "[x](https://x.test)"
        );
        assert_eq!(
            normalize_wrapped_links("`[x]\\(https://x.test\\)`", |_, _| true),
            "`[x]\\(https://x.test\\)`"
        );
        assert_eq!(
            trim_bare_url("https://x.test/a_(b))."),
            "https://x.test/a_(b)"
        );
    }

    #[test]
    fn footnotes_number_and_follow_the_body() {
        let blocks = blocks("See[^a] and[^b].\n\n[^b]: Bee\n[^a]: Ay");
        assert_eq!(
            blocks[0],
            Block::Paragraph("See![1](<buzz-footnote:1>) and![2](<buzz-footnote:2>)\\.".into())
        );
        let Block::Footnotes(notes) = &blocks[1] else {
            panic!("no footnotes");
        };
        assert_eq!(
            notes[0],
            vec![Block::Paragraph("Ay [↩](<buzz-footnote-back:1>)".into())]
        );
    }

    #[test]
    fn links_copy_with_their_destination() {
        let directory = directory();
        assert_eq!(
            copy_text("https://x.test", "site", &directory),
            "site (https://x.test)"
        );
        assert_eq!(
            copy_text("https://x.test", "https://x.test", &directory),
            "https://x.test"
        );
    }

    #[test]
    fn shows_raw_html_as_the_typed_text() {
        let visible = |content| plain_markdown(&paragraph(content));
        assert_eq!(visible("a <b>b</b> c"), "a <b>b</b> c");
        assert_eq!(
            visible("<harness stuff goes here>"),
            "<harness stuff goes here>"
        );
        assert_eq!(visible("run the <harness> now"), "run the <harness> now");
        assert_eq!(
            paragraph("**<harness>** and _<b>x</b>_"),
            "**\\<harness\\>** and *\\<b\\>x\\<\\/b\\>*"
        );
        // A block keeps its lines and is never Markdown or emoji; a
        // complete comment stays hidden.
        let mut block = row("<div onclick=\"x()\">\n**not bold** :party:\n</div>\n\n<!-- note -->");
        block.emoji = vec![CustomEmoji {
            shortcode: "party".into(),
            url: "https://x.test/p.png".into(),
        }];
        assert_eq!(
            prepare(&block, &directory()).body,
            Document::Blocks(vec![Block::Paragraph(
                "\\<div onclick\\=\\\"x\\(\\)\\\"\\>\\\n\\*\\*not bold\\*\\* \\:party\\:\\\n\\<\\/div\\>"
                    .into()
            )])
        );
        // Buzz Desktop's wave marker.
        assert_eq!(
            visible("<!-- buzz:wave:v1 -->\nLogan waved at you."),
            "Logan waved at you."
        );
        assert_eq!(visible("a <!-- hidden --> b"), "a  b");
        assert_eq!(paragraph("<!-- x --> <harness>"), " \\<harness\\>");
        assert_eq!(
            visible("an <!-- unclosed comment"),
            "an <!-- unclosed comment"
        );
        assert!(blocks("<!-- buzz:wave:v1 -->").is_empty());
        // Inside a spoiler it stays inline text.
        let prepared = prepare(&row("Before ||<harness>|| after"), &directory());
        assert_eq!(
            prepared.body,
            Document::Blocks(vec![Block::Paragraph(
                "Before ![\\<harness\\>](<buzz-spoiler:0>) after".into()
            )])
        );
        assert_eq!(&*prepared.spoilers, &[SharedString::from("\\<harness\\>")]);
    }

    #[test]
    fn keeps_structure() {
        assert_eq!(
            blocks("# T\n\n> q\n\n- [x] done\n\n```\nfn main() {}\n```\n\n---"),
            vec![
                Block::Heading(1, "T".into()),
                Block::Quote(vec![Block::Paragraph("q".into())]),
                Block::List {
                    ordered: false,
                    start: 1,
                    items: vec![Item {
                        checked: Some(true),
                        blocks: vec![Block::Paragraph("done".into())],
                    }],
                },
                Block::Code("fn main() {}".into()),
                Block::Rule,
            ]
        );
    }

    #[test]
    fn detects_emoji_only_bodies() {
        let party = CustomEmoji {
            shortcode: "party".into(),
            url: "https://x.test/p.png".into(),
        };
        assert!(is_emoji_only("🎉 🚀", &[]));
        assert!(is_emoji_only("👍🏽 ❤️ 1️⃣ 🇺🇸 👨‍👩‍👧", &[]));
        assert!(is_emoji_only(" :party: 🎉", std::slice::from_ref(&party)));
        assert!(!is_emoji_only(":party:", &[]));
        assert!(is_emoji_only_by(" :Party: 🎉", |code| code == "party"));
        assert!(!is_emoji_only_by(":party: hi", |code| code == "party"));
        assert!(!is_emoji_only("🎉 yay", &[]));
        assert!(!is_emoji_only("1", &[]));
        assert!(!is_emoji_only("", &[]));
    }

    #[test]
    fn falls_back_to_plain_text_when_too_deep() {
        let body = "> ".repeat(200) + "deep";
        assert_eq!(
            prepare(&row(&body), &directory()).body,
            Document::Plain(body)
        );
    }

    #[test]
    fn paragraphs_keep_breaks_and_escape_prose() {
        assert_eq!(
            blocks("a *b* 1.\nc"),
            vec![Block::Paragraph("a *b* 1\\.\\\nc".into())]
        );
    }

    #[test]
    fn table_cells_keep_inline_content() {
        let blocks = blocks("| a | b |\n|---|---|\n| [x](https://x.test) | `c` |");
        let Block::Table { rows, .. } = &blocks[0] else {
            panic!("not a table");
        };
        assert_eq!(rows[1][0], "[x](<https://x.test>)");
        assert_eq!(rows[1][1], "`c`");
    }
}
