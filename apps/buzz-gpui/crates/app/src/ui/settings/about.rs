//! Settings → About: the version, the license and the attribution notice
//! shipped in `NOTICE.md`.

use super::{form_group, header, inline_header, preference_row, rows_group, section};
use buzz_ui::{ActiveTheme, TypeStyled};
use gpui_kit::*;

const NOTICE: &str = include_str!("../../../../../NOTICE.md");

pub(super) fn render(cx: &App) -> AnyElement {
    let version = env!("CARGO_PKG_VERSION");
    let info = rows_group(
        vec![
            preference_row(
                None,
                "Version",
                Some(version.into_any_element()),
                Vec::new(),
                cx,
            )
            .into_any_element(),
            preference_row(
                None,
                "License",
                Some(
                    "Apache-2.0. A native port of buzz-app (Apache-2.0, © Block, Inc.)."
                        .into_any_element(),
                ),
                Vec::new(),
                cx,
            )
            .into_any_element(),
        ],
        cx,
    );
    section(vec![
        header("About", Some(format!("Buzz GPUI {version}").into()), cx).into_any_element(),
        info.into_any_element(),
        div()
            .child(inline_header(
                "Notices",
                Some("Software, fonts and artwork this app includes.".into()),
                Vec::new(),
                cx,
            ))
            .child(form_group(notice(cx), cx))
            .into_any_element(),
    ])
}

/// `NOTICE.md` as headings and paragraphs, with link targets and code
/// marks dropped.
fn notice(cx: &App) -> Vec<AnyElement> {
    let c = cx.colors();
    blocks(NOTICE)
        .into_iter()
        .map(|block| match block {
            Block::Heading(text) => div().text_label_sm().child(text).into_any_element(),
            Block::Paragraph(text) => div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element(),
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Block {
    Heading(String),
    Paragraph(String),
}

fn blocks(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(plain(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    for line in markdown.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix('#') {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading(plain(
                heading.trim_start_matches('#').trim(),
            )));
        } else if line.is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else {
            paragraph.push(line);
        }
    }
    flush(&mut paragraph, &mut blocks);
    blocks
}

/// `[text](target)` becomes `text`; backticks go.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let Some(close) = rest[start..].find("](") else {
            break;
        };
        let Some(end) = rest[start + close..].find(')') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&rest[start + 1..start + close]);
        rest = &rest[start + close + end + 1..];
    }
    out.push_str(rest);
    out.replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn flattens_markdown() {
        let blocks =
            blocks("# Title\n\nSee [LICENSE](LICENSE) and\n`assets/` here.\n\n## Fonts\nInter");
        assert_eq!(
            blocks,
            vec![
                Block::Heading("Title".into()),
                Block::Paragraph("See LICENSE and assets/ here.".into()),
                Block::Heading("Fonts".into()),
                Block::Paragraph("Inter".into()),
            ]
        );
    }
}
