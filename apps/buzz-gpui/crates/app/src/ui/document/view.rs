//! Draws a prepared GitHub body (`GitHubBody.tsx`, `.body` in
//! `GitHub.module.css`). Paragraphs are blocks, so players and previews
//! can sit inside prose; links keep the surrounding text colour,
//! underlined, and open in the browser.

use super::attachments::{Attachment, MediaKind, Metadata};
use super::body::{self, Block, Body, Inline};
use crate::ui::media::{self, ImageStatus};
use buzz_relay_client::Media;
use buzz_relay_client::fold::{self, AttachmentKind};
use buzz_ui::{ActiveTheme, Colors, Icon, IconName, TypeStyled, fonts, tokens::font};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use markdown::mdast::AlignKind;
use std::rc::Rc;

/// What a body is drawn with.
#[derive(Clone)]
pub struct Context {
    /// Unique per body on screen.
    pub id: SharedString,
    pub media: Media,
}

/// A drawn body and its outer block margins, which collapse with the
/// blocks around it as CSS margins do.
pub struct Drawn {
    pub element: AnyElement,
    pub top: f32,
    pub bottom: f32,
}

pub fn draw(body: &Body, context: &Context, cx: &App) -> Drawn {
    let c = *cx.colors();
    match body {
        Body::Plain(text) => Drawn {
            element: div()
                .w_full()
                .text_color(c.text_standard)
                .child(text.clone())
                .into_any_element(),
            top: 0.,
            bottom: 0.,
        },
        Body::Rich { blocks, metadata } => {
            let mut out = Out {
                context,
                metadata,
                c,
                color: c.text_standard,
            };
            let (element, top, bottom) = column(blocks, &context.id, true, &mut out);
            Drawn {
                element: element.w_full().min_w_0().into_any_element(),
                top,
                bottom,
            }
        }
    }
}

struct Out<'a> {
    context: &'a Context,
    metadata: &'a Metadata,
    c: Colors,
    /// The text colour here: subtle inside quotes.
    color: Hsla,
}

/// CSS block margins (top, bottom).
fn margins(block: &Block) -> (f32, f32) {
    match block {
        Block::Heading(_) => (16., 8.),
        Block::Table { .. } | Block::Rule => (0., 0.),
        _ => (12., 12.),
    }
}

/// Blocks in a column, adjacent margins collapsed. Returns the column
/// and its first and last margins, which the caller places.
fn column(blocks: &[Block], path: &str, spread: bool, out: &mut Out) -> (Div, f32, f32) {
    let mut column = div().flex().flex_col().min_w_0();
    let mut previous: Option<f32> = None;
    let mut first = 0.;
    for (index, block) in blocks.iter().enumerate() {
        let (top, bottom) = if spread { margins(block) } else { (0., 0.) };
        let gap = match previous {
            Some(previous) => f32::max(previous, top),
            None => {
                first = top;
                0.
            }
        };
        let element = render_block(block, &format!("{path}-{index}"), spread, out);
        column = column.child(div().min_w_0().pt(px(gap)).child(element));
        previous = Some(bottom);
    }
    (column, first, previous.unwrap_or(0.))
}

fn render_block(block: &Block, path: &str, spread: bool, out: &mut Out) -> AnyElement {
    let c = out.c;
    match block {
        Block::Paragraph(inlines) => paragraph(inlines, path, out),
        Block::Heading(inlines) => div()
            .text_body_lg()
            .font_weight(FontWeight::NORMAL)
            .child(paragraph(inlines, path, out))
            .into_any_element(),
        Block::Quote(blocks) => {
            let color = std::mem::replace(&mut out.color, c.text_subtle);
            let (inner, top, bottom) = column(blocks, path, spread, out);
            out.color = color;
            // The quote has no vertical padding, so its children's margins
            // pass through it; inside they would double up.
            let _ = (top, bottom);
            div()
                .pl(px(12.))
                .border_l_2()
                .border_color(c.border_standard)
                .text_color(c.text_subtle)
                .child(inner)
                .into_any_element()
        }
        Block::List {
            ordered,
            start,
            spread: loose,
            items,
        } => list(*ordered, *start, *loose, items, path, out),
        Block::Code(code) => div()
            .id(SharedString::from(format!("{}-code{path}", out.context.id)))
            .w_full()
            .p(px(12.))
            .bg(c.surface_panel)
            .overflow_x_scroll()
            .font_family(font::MONO)
            .text_color(out.color)
            .child(
                div()
                    .whitespace_nowrap()
                    .child(code.trim_end_matches('\n').to_string()),
            )
            .into_any_element(),
        Block::Table { align, rows } => table(align, rows, path, out),
        Block::Rule => div()
            .w_full()
            .h(px(1.))
            .bg(c.border_standard)
            .into_any_element(),
        Block::Footnotes(notes) => {
            let items: Vec<body::Item> = notes
                .iter()
                .map(|blocks| body::Item {
                    checked: None,
                    blocks: blocks.clone(),
                })
                .collect();
            list(true, 1, false, &items, path, out)
        }
    }
}

fn list(
    ordered: bool,
    start: u32,
    loose: bool,
    items: &[body::Item],
    path: &str,
    out: &mut Out,
) -> AnyElement {
    let c = out.c;
    let task = items.iter().any(|item| item.checked.is_some());
    let mut list = div().flex().flex_col().min_w_0();
    let mut previous: Option<f32> = None;
    for (index, item) in items.iter().enumerate() {
        let (content, top, bottom) = column(&item.blocks, &format!("{path}-{index}"), loose, out);
        let gap = previous.map_or(0., |previous| f32::max(previous, top));
        previous = Some(bottom);
        let marker = match item.checked {
            Some(checked) => checkbox(checked, &c).into_any_element(),
            None if task => div().into_any_element(),
            None if ordered => div()
                .child(format!("{}.", start as usize + index))
                .into_any_element(),
            None => div().child("•").into_any_element(),
        };
        let marker = if item.checked.is_some() {
            div().flex_none().mr(px(4.)).child(marker)
        } else if task {
            div().flex_none()
        } else {
            // Outside markers hang in the 20px list padding.
            div()
                .flex_none()
                .w(px(20.))
                .ml(px(-20.))
                .pr(px(6.))
                .flex()
                .justify_end()
                .child(marker)
        };
        list = list.child(
            div()
                .flex()
                .pt(px(gap))
                .text_color(out.color)
                .child(marker)
                .child(div().flex_1().min_w_0().child(content)),
        );
    }
    div()
        .when(!task, |d| d.pl(px(20.)))
        .child(list)
        .into_any_element()
}

/// A task list's read-only checkbox.
fn checkbox(checked: bool, c: &Colors) -> impl IntoElement {
    div()
        .mt(px(3.))
        .size(px(13.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .border_color(c.border_prominent)
        .when(checked, |d| {
            d.bg(c.affordance_prominent)
                .border_color(c.affordance_prominent)
                .child(Icon::new(IconName::Check).size(10.).color(c.text_inverse))
        })
}

fn table(align: &[AlignKind], rows: &[Vec<Vec<Inline>>], path: &str, out: &mut Out) -> AnyElement {
    let c = out.c;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut frame = div()
        .flex()
        .border_t_1()
        .border_l_1()
        .border_color(c.border_standard);
    for column in 0..columns {
        let mut cells = div().flex().flex_col().flex_none();
        for (index, row) in rows.iter().enumerate() {
            let head = index == 0;
            let content = row.get(column).cloned().unwrap_or_default();
            let cell = div()
                .p(px(8.))
                .flex()
                .border_r_1()
                .border_b_1()
                .border_color(c.border_standard)
                .whitespace_nowrap()
                .when(head, |d| d.font_weight(FontWeight::BOLD))
                .map(|d| match align.get(column) {
                    Some(AlignKind::Center) => d.justify_center(),
                    Some(AlignKind::Right) => d.justify_end(),
                    Some(AlignKind::Left) => d,
                    _ if head => d.justify_center(),
                    _ => d,
                })
                .child(text_line(
                    &content,
                    &format!("{path}-{index}-{column}"),
                    head,
                    out,
                ));
            cells = cells.child(cell);
        }
        frame = frame.child(cells);
    }
    div()
        .id(SharedString::from(format!(
            "{}-table{path}",
            out.context.id
        )))
        .max_w_full()
        .overflow_x_scroll()
        .child(div().flex().child(frame))
        .into_any_element()
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Marks {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}

/// What a paragraph lays out: runs of text, and blocks between them.
enum Piece {
    Text(String, Marks, Option<String>),
    Media {
        attachment: Attachment,
        label: Vec<Inline>,
        image: bool,
        description: String,
    },
    LinkedImage {
        children: Vec<Inline>,
        href: String,
    },
}

fn flatten(
    inlines: &[Inline],
    marks: Marks,
    link: Option<&str>,
    metadata: &Metadata,
    out: &mut Vec<Piece>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => {
                out.push(Piece::Text(text.clone(), marks, link.map(str::to_string)))
            }
            Inline::Code(text) => out.push(Piece::Text(
                text.clone(),
                Marks {
                    code: true,
                    ..marks
                },
                link.map(str::to_string),
            )),
            Inline::Strong(children) => flatten(
                children,
                Marks {
                    bold: true,
                    ..marks
                },
                link,
                metadata,
                out,
            ),
            Inline::Emphasis(children) => flatten(
                children,
                Marks {
                    italic: true,
                    ..marks
                },
                link,
                metadata,
                out,
            ),
            Inline::Delete(children) => flatten(
                children,
                Marks {
                    strike: true,
                    ..marks
                },
                link,
                metadata,
                out,
            ),
            Inline::Link {
                url: None,
                children,
            } => flatten(children, marks, link, metadata, out),
            Inline::Link {
                url: Some(url),
                children,
            } => {
                if body::contains_image(children) {
                    out.push(Piece::LinkedImage {
                        children: children.clone(),
                        href: url.clone(),
                    });
                } else if let Some(attachment) = body::media(url, metadata, false) {
                    out.push(Piece::Media {
                        attachment,
                        label: children.clone(),
                        image: false,
                        description: body::plain_text(children),
                    });
                } else {
                    flatten(children, marks, Some(url), metadata, out);
                }
            }
            Inline::Image { url: None, alt } => {
                out.push(Piece::Text(alt.clone(), marks, link.map(str::to_string)))
            }
            Inline::Image {
                url: Some(url),
                alt,
            } => match body::media(url, metadata, true) {
                Some(attachment) => out.push(Piece::Media {
                    attachment,
                    label: vec![Inline::Text(alt.clone())],
                    image: true,
                    description: alt.clone(),
                }),
                None => out.push(Piece::Text(
                    if alt.is_empty() {
                        "Open attachment".into()
                    } else {
                        alt.clone()
                    },
                    marks,
                    Some(url.clone()),
                )),
            },
        }
    }
}

/// A paragraph: its text runs and the players between them.
fn paragraph(inlines: &[Inline], path: &str, out: &Out) -> AnyElement {
    let mut pieces = Vec::new();
    flatten(inlines, Marks::default(), None, out.metadata, &mut pieces);
    pieces_column(pieces, path, out)
}

fn pieces_column(pieces: Vec<Piece>, path: &str, out: &Out) -> AnyElement {
    let mut column = div().flex().flex_col().min_w_0();
    let mut run: Vec<(String, Marks, Option<String>)> = Vec::new();
    let mut index = 0;
    let mut first = true;
    let flush = |run: &mut Vec<(String, Marks, Option<String>)>,
                 column: Div,
                 first: &mut bool,
                 index: &mut usize,
                 at_block: bool|
     -> Div {
        let mut text = std::mem::take(run);
        // A line break closing a block's last line adds no line, and
        // spaces next to a block are not drawn.
        if let Some(last) = text.last_mut()
            && last.0.ends_with('\n')
        {
            last.0.pop();
        }
        if at_block || !*first {
            trim(&mut text);
        }
        if text.iter().all(|(t, _, _)| t.trim_matches(' ').is_empty()) {
            return column;
        }
        *index += 1;
        let gap = if *first { 0. } else { 12. };
        *first = false;
        column.child(
            div()
                .pt(px(gap))
                .child(rich_text(&text, &format!("{path}-t{index}"), out)),
        )
    };
    for piece in pieces {
        match piece {
            Piece::Text(text, marks, link) => run.push((text, marks, link)),
            block => {
                column = flush(&mut run, column, &mut first, &mut index, true);
                index += 1;
                let gap = if first { 0. } else { 12. };
                first = false;
                column = column.child(div().pt(px(gap)).child(match block {
                    Piece::Media {
                        attachment,
                        label,
                        image,
                        description,
                    } => embedded(
                        attachment,
                        label,
                        image,
                        description,
                        &format!("{path}-m{index}"),
                        out,
                    ),
                    Piece::LinkedImage { children, href } => {
                        linked_image(&children, href, &format!("{path}-l{index}"), out)
                    }
                    Piece::Text(..) => unreachable!(),
                }));
            }
        }
    }
    flush(&mut run, column, &mut first, &mut index, false).into_any_element()
}

/// Trims spaces at the edges of a run of text that meets a block.
fn trim(run: &mut [(String, Marks, Option<String>)]) {
    if let Some(first) = run.first_mut() {
        first.0 = first.0.trim_start_matches(' ').to_string();
    }
    if let Some(last) = run.last_mut() {
        last.0 = last.0.trim_end_matches(' ').to_string();
    }
}

/// One line of text runs, unwrapped (table cells).
fn text_line(inlines: &[Inline], path: &str, bold: bool, out: &Out) -> AnyElement {
    let mut pieces = Vec::new();
    flatten(
        inlines,
        Marks {
            bold,
            ..Marks::default()
        },
        None,
        out.metadata,
        &mut pieces,
    );
    let runs: Vec<_> = pieces
        .into_iter()
        .map(|piece| match piece {
            Piece::Text(text, marks, link) => (text, marks, link),
            Piece::Media { label, .. } => (body::plain_text(&label), Marks::default(), None),
            Piece::LinkedImage { href, .. } => (href.clone(), Marks::default(), Some(href)),
        })
        .collect();
    if runs.iter().all(|(t, _, _)| t.is_empty()) {
        return div().into_any_element();
    }
    rich_text(&runs, path, out)
}

/// Styled, clickable text.
fn rich_text(pieces: &[(String, Marks, Option<String>)], path: &str, out: &Out) -> AnyElement {
    let mut text = String::new();
    let mut runs = Vec::new();
    let mut ranges = Vec::new();
    let mut urls = Vec::new();
    for (piece, marks, link) in pieces {
        if piece.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(piece);
        let mut font = gpui_kit::font(if marks.code { font::MONO } else { font::SANS });
        font.fallbacks = Some(fonts::fallbacks());
        if marks.bold {
            font.weight = FontWeight::BOLD;
        }
        if marks.italic {
            font.style = FontStyle::Italic;
        }
        runs.push(TextRun {
            len: piece.len(),
            font,
            color: out.color,
            background_color: None,
            underline: link.as_ref().map(|_| UnderlineStyle {
                thickness: px(1.),
                color: Some(out.color),
                wavy: false,
            }),
            strikethrough: marks.strike.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(out.color),
            }),
        });
        if let Some(url) = link {
            ranges.push(start..text.len());
            urls.push(url.clone());
        }
    }
    let styled = StyledText::new(text).with_runs(runs);
    let id = SharedString::from(format!("{}-text{path}", out.context.id));
    if ranges.is_empty() {
        return div().id(id).child(styled).into_any_element();
    }
    InteractiveText::new(id, styled)
        .on_click(ranges, move |index, _, cx| {
            if let Some(url) = urls.get(index) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

/// A plain link line (`.attachment > a`).
fn link_line(label: &[Inline], fallback: &str, url: &str, path: &str, out: &Out) -> AnyElement {
    let mut pieces = Vec::new();
    flatten(
        label,
        Marks::default(),
        Some(url),
        out.metadata,
        &mut pieces,
    );
    let mut runs: Vec<_> = pieces
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Text(text, marks, _) => Some((text, marks, Some(url.to_string()))),
            _ => None,
        })
        .collect();
    if runs.iter().all(|(t, _, _)| t.is_empty()) {
        runs = vec![(
            fallback.to_string(),
            Marks::default(),
            Some(url.to_string()),
        )];
    }
    rich_text(&runs, path, out)
}

fn timeline_attachment(attachment: &Attachment) -> fold::Attachment {
    fold::Attachment {
        url: attachment.url.clone(),
        kind: match attachment.kind {
            MediaKind::Image => AttachmentKind::Image,
            MediaKind::Video => AttachmentKind::Video,
            MediaKind::Audio => AttachmentKind::Audio,
        },
        mime: None,
        size: None,
        name: attachment.name.clone(),
        duration: None,
        waveform: None,
        voice_note: false,
        dimensions: None,
        blurhash: None,
        preview_url: None,
        insecure: false,
    }
}

/// A GitHub upload in the body (`EmbeddedAttachment`): its player or
/// preview, then its link until the media loads, or its label as a
/// caption after.
fn embedded(
    attachment: Attachment,
    label: Vec<Inline>,
    image: bool,
    description: String,
    path: &str,
    out: &Out,
) -> AnyElement {
    let context = out.context.clone();
    let id = SharedString::from(format!("{}-media{path}", context.id));
    let metadata = out.metadata.clone();
    let (c, color) = (out.c, out.color);
    let path = path.to_string();
    media::Lazy::new(id.clone(), move |window, cx| {
        let out = Out {
            context: &context,
            metadata: &metadata,
            c,
            color,
        };
        let source = attachment.url.clone();
        let fallback_label = attachment
            .name
            .clone()
            .unwrap_or_else(|| "Open attachment".into());
        let timeline = timeline_attachment(&attachment);
        let (player, loaded) = match attachment.kind {
            MediaKind::Image => {
                let status = media::image_status(&source, &context.media, window, cx);
                let open: media::Open = {
                    let (timeline, media) = (timeline.clone(), context.media.clone());
                    Rc::new(move |window, cx| {
                        media::open_media(timeline.clone(), &media, window, cx)
                    })
                };
                let description = if description.is_empty() && !image {
                    "Attachment preview".to_string()
                } else {
                    description.clone()
                };
                (
                    media::image_preview(
                        "preview",
                        &source,
                        description.into(),
                        &context.media,
                        open,
                    )
                    .into_any_element(),
                    status == ImageStatus::Loaded,
                )
            }
            MediaKind::Video => {
                let open: media::OpenReview = {
                    let (timeline, media) = (timeline.clone(), context.media.clone());
                    Rc::new(move |seconds, inline, window, cx| {
                        media::open_media_at(
                            timeline.clone(),
                            &media,
                            seconds,
                            Some(inline),
                            window,
                            cx,
                        )
                    })
                };
                (
                    media::video_preview("video", &timeline, &context.media, None, open)
                        .into_any_element(),
                    false,
                )
            }
            MediaKind::Audio => (
                media::audio_attachment("audio", &timeline, &context.media, cx),
                false,
            ),
        };
        let tail = if loaded {
            (!image && body::captioned(&label, &source)).then(|| {
                div()
                    .child(paragraph(&label, &format!("{path}-c"), &out))
                    .into_any_element()
            })
        } else {
            Some(
                div()
                    .mt(px(4.))
                    .child(link_line(
                        &label,
                        &fallback_label,
                        &source,
                        &format!("{path}-f"),
                        &out,
                    ))
                    .into_any_element(),
            )
        };
        div()
            .flex()
            .flex_col()
            .items_start()
            .min_w_0()
            .child(player)
            .children(tail)
            .into_any_element()
    })
    .into_any_element()
}

/// A link whose label holds an image: the label, then the destination
/// as its own link, so a preview never nests inside a link.
fn linked_image(children: &[Inline], href: String, path: &str, out: &Out) -> AnyElement {
    let mut pieces = Vec::new();
    flatten(children, Marks::default(), None, out.metadata, &mut pieces);
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(pieces_column(pieces, &format!("{path}-a"), out))
        .child(rich_text(
            &[(href.clone(), Marks::default(), Some(href))],
            &format!("{path}-h"),
            out,
        ))
        .into_any_element()
}
