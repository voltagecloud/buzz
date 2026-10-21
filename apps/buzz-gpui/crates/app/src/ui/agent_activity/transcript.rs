//! The turn-by-turn transcript (`ActivityTranscript.tsx` and its CSS),
//! shown by the Agent Activity panel and the agent types' session
//! transcripts.

use super::panel::{ActivityPanel, code_block, date_time};
use crate::agent_activity::transcript_model::{
    Item, ItemKind, Section, ToolDiff, ToolItem, Transcript, TranscriptTurn, duration,
};
use crate::agent_activity::turns::TurnState;
use crate::community::Community;
use crate::ui::markdown::{self, Directory};
use buzz_relay_client::fold::MessageRow;
use buzz_ui::tokens::{font, radius};
use buzz_ui::{ActiveTheme, Icon, IconName, Tooltip, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A view that shows a transcript: it keeps which disclosures are open,
/// and names the community whose names and links replies render with.
pub trait TranscriptHost: Sized + 'static {
    fn expanded(&mut self) -> &mut HashSet<String>;
    /// `None` shows replies as plain text.
    fn community(&self, cx: &App) -> Option<Entity<Community>>;
}

impl TranscriptHost for ActivityPanel {
    fn expanded(&mut self) -> &mut HashSet<String> {
        &mut self.expanded
    }
    fn community(&self, _: &App) -> Option<Entity<Community>> {
        Some(self.community.clone())
    }
}

/// Opens or closes the disclosure `key`.
fn toggle<T: TranscriptHost>(this: &mut T, key: String, cx: &mut Context<T>) {
    let expanded = this.expanded();
    if !expanded.remove(&key) {
        expanded.insert(key);
    }
    cx.notify();
}

/// ACP's standard tool kinds; tool names stay as the adapter sent them.
fn tool_icon(kind: &str) -> IconName {
    match kind {
        "execute" => IconName::TerminalWindow,
        "read" => IconName::Eye,
        "edit" | "move" => IconName::PencilSimple,
        "delete" => IconName::Trash,
        "search" => IconName::MagnifyingGlass,
        "fetch" => IconName::Globe,
        "think" => IconName::Bulb,
        _ => IconName::Wrench,
    }
}

fn tool_status(status: &str) -> Option<&'static str> {
    Some(match status {
        "pending" => "Pending",
        "in_progress" => "Running",
        "failed" => "Failed",
        _ => return None,
    })
}

fn first_line(text: &str) -> String {
    text.split('\n')
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn thread_fact(turn: &TranscriptTurn) -> String {
    match &turn.thread_root {
        Some(None) => "Channel conversation".into(),
        Some(Some(root)) => format!("Thread {}", &root[..8.min(root.len())]),
        None => "Thread unknown".into(),
    }
}

/// A file's lines; a final newline does not start another.
fn lines(text: &str) -> Vec<&str> {
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

/// The changed hunk between the unchanged leading and trailing lines.
pub fn diff_hunk(diff: &ToolDiff) -> (Vec<String>, Vec<String>) {
    let before = diff.old_text.as_deref().map(lines).unwrap_or_default();
    let after = lines(&diff.new_text);
    let mut head = 0;
    while head < before.len() && head < after.len() && before[head] == after[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < before.len() - head
        && tail < after.len() - head
        && before[before.len() - 1 - tail] == after[after.len() - 1 - tail]
    {
        tail += 1;
    }
    (
        before[head..before.len() - tail]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        after[head..after.len() - tail]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
}

/// An item row: a 16px icon beside wrapping content.
fn row(icon: IconName, color: Hsla, content: impl IntoElement) -> Div {
    div()
        .flex()
        .items_start()
        .gap(rems(0.5))
        .min_w_0()
        .child(
            div()
                .flex_none()
                .mt(rems(0.125))
                .child(Icon::new(icon).size(16.).color(color)),
        )
        .child(div().flex_1().min_w_0().child(content))
}

pub fn timeline<T: TranscriptHost>(
    this: &mut T,
    transcript: &Arc<Transcript>,
    agent_name: &str,
    states: &HashMap<String, TurnState>,
    before: Vec<AnyElement>,
    cx: &mut Context<T>,
) -> AnyElement {
    let mut children = before;
    if transcript.turns.is_empty() {
        children.push(
            div()
                .text_body_sm()
                .text_color(cx.colors().text_subtle)
                .child("No turns captured for this scope yet.")
                .into_any_element(),
        );
    }
    for turn in &transcript.turns {
        let state = if turn.ended_at.is_some() {
            TurnState::Ended
        } else {
            states
                .get(&turn.turn_id)
                .copied()
                .unwrap_or(TurnState::Unknown)
        };
        children.push(turn_view(this, turn, state, agent_name, cx));
    }
    div()
        .flex()
        .flex_col()
        .gap(rems(1.))
        .min_w_0()
        .children(children)
        .into_any_element()
}

fn turn_view<T: TranscriptHost>(
    this: &mut T,
    turn: &TranscriptTurn,
    state: TurnState,
    agent_name: &str,
    cx: &mut Context<T>,
) -> AnyElement {
    let c = *cx.colors();
    let (status, color) = if turn.error.is_some() {
        ("Error", c.text_danger)
    } else if state == TurnState::Working {
        ("Working", c.affordance_accent_prominent)
    } else if turn.ended_at.is_some() {
        ("Completed", c.text_subtle)
    } else {
        ("Status unknown", c.text_subtle)
    };
    let mut facts: Vec<String> = Vec::new();
    if let Some(ended) = turn.ended_at {
        facts.push(duration(ended.saturating_sub(turn.started_at)));
    }
    facts.push(thread_fact(turn));
    if turn.source.as_deref() == Some("heartbeat") {
        facts.push("heartbeat".into());
    }
    if turn.new_session {
        facts.push("new session".into());
    }
    facts.extend(turn.stop_reason.clone());
    if let Some((used, size)) = turn.context {
        facts.push(format!(
            "context {}k/{}k",
            (used / 1000.).round(),
            (size / 1000.).round()
        ));
    }
    let mut session = vec!["Turn".to_string(), date_time(turn.started_at)];
    session.extend(turn.config.iter().cloned());
    let mut children: Vec<AnyElement> = vec![
        div()
            .pt(rems(0.5))
            .border_t_1()
            .border_color(c.border_standard)
            .text_body_sm()
            .text_color(c.text_subtle)
            .text_center()
            .child(session.join(" · "))
            .into_any_element(),
        {
            // The state is colored; the facts wrap after it.
            let text: String = std::iter::once(status.to_string())
                .chain(facts.iter().map(|fact| format!(" · {fact}")))
                .collect();
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(StyledText::new(text).with_highlights([(
                    0..status.len(),
                    HighlightStyle {
                        color: Some(color),
                        ..Default::default()
                    },
                )]))
                .into_any_element()
        },
    ];
    if turn.partial {
        children.push(
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("The start of this turn isn't loaded.")
                .into_any_element(),
        );
    }
    for item in &turn.items {
        children.push(item_view(this, &turn.turn_id, item, agent_name, cx));
    }
    if let Some(error) = &turn.error {
        children.push(
            row(
                IconName::WarningCircle,
                c.text_danger,
                div().text_color(c.text_danger).child(error.clone()),
            )
            .into_any_element(),
        );
    }
    if state == TurnState::Working {
        children.push(
            row(
                IconName::DotsThree,
                c.text_subtle,
                div().text_color(c.text_subtle).child("Working…"),
            )
            .into_any_element(),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .min_w_0()
        .children(children)
        .into_any_element()
}

/// A `<details>`-like disclosure: a summary line toggling the body.
fn disclosure<T: TranscriptHost>(
    this: &mut T,
    key: String,
    summary: AnyElement,
    body: impl FnOnce(&mut T, &mut Context<T>) -> Vec<AnyElement>,
    cx: &mut Context<T>,
) -> AnyElement {
    let open = this.expanded().contains(&key);
    let toggle_key = key.clone();
    let body = open.then(|| body(this, cx));
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(
            div()
                .id(SharedString::from(key))
                .cursor_pointer()
                .min_w_0()
                .child(summary)
                .on_click(
                    cx.listener(move |this: &mut T, _, _, cx| toggle(this, toggle_key.clone(), cx)),
                ),
        )
        .children(body.into_iter().flatten())
        .into_any_element()
}

fn sections_view<T: TranscriptHost>(
    this: &mut T,
    key: String,
    title: &str,
    sections: &[Section],
    cx: &mut Context<T>,
) -> AnyElement {
    let c = *cx.colors();
    let summary = div()
        .text_body_sm()
        .text_color(c.text_subtle)
        .child(format!("{title} · {} sections", sections.len()))
        .into_any_element();
    let sections = sections.to_vec();
    let base = key.clone();
    disclosure(
        this,
        key,
        summary,
        move |this, cx| {
            sections
                .iter()
                .enumerate()
                .map(|(index, section)| {
                    let summary = div()
                        .text_body_sm()
                        .child(section.tag.clone())
                        .into_any_element();
                    let body = section.body.clone();
                    div()
                        .ml(rems(1.))
                        .child(disclosure(
                            this,
                            format!("{base}:{index}"),
                            summary,
                            move |_, cx| vec![code_block(&body, cx)],
                            cx,
                        ))
                        .into_any_element()
                })
                .collect()
        },
        cx,
    )
}

fn item_view<T: TranscriptHost>(
    this: &mut T,
    turn: &str,
    item: &Item,
    agent_name: &str,
    cx: &mut Context<T>,
) -> AnyElement {
    let c = *cx.colors();
    let key = format!("{turn}:{}", item.id);
    match &item.kind {
        ItemKind::System(sections) => {
            sections_view(this, format!("system:{key}"), "System prompt", sections, cx)
        }
        ItemKind::Prompt {
            text,
            author,
            sections,
            steer,
        } => {
            // "**Author** *(steer)* text" in one wrapping run.
            let mut line = String::new();
            let mut highlights = Vec::new();
            if let Some(author) = author {
                highlights.push((
                    0..author.len(),
                    HighlightStyle {
                        font_weight: Some(FontWeight::SEMIBOLD),
                        ..Default::default()
                    },
                ));
                line.push_str(author);
                line.push(' ');
            }
            if *steer {
                let start = line.len();
                line.push_str("(steer)");
                highlights.push((
                    start..line.len(),
                    HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    },
                ));
                line.push(' ');
            }
            line.push_str(text);
            let line = StyledText::new(line).with_highlights(highlights);
            let mut out = vec![row(IconName::User, c.text_standard, line).into_any_element()];
            if sections.len() > 1 {
                out.push(sections_view(
                    this,
                    format!("prompt:{key}"),
                    "Prompt context",
                    sections,
                    cx,
                ));
            }
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .children(out)
                .into_any_element()
        }
        ItemKind::Thought { text, .. } => row(
            IconName::Bulb,
            c.text_subtle,
            div().text_color(c.text_subtle).child(text.clone()),
        )
        .into_any_element(),
        ItemKind::Message { text, posted, .. } => {
            let body = reply_view(this, text, &key, cx);
            row(
                IconName::ChatCircle,
                c.text_standard,
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(agent_name.to_string()),
                            )
                            .when(*posted, |this| {
                                this.child(
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_subtle)
                                        .ml(rems(0.25))
                                        .child("· Posted"),
                                )
                            }),
                    )
                    .child(body),
            )
            .into_any_element()
        }
        ItemKind::Tool(tool) => tool_view(this, format!("tool:{key}"), tool, item.at, cx),
        ItemKind::Plan(entries) => row(
            IconName::ListChecks,
            c.text_standard,
            div()
                .flex()
                .flex_col()
                .children(entries.iter().map(|entry| {
                    let done = entry.status == "completed";
                    div().child(format!(
                        "{} {}",
                        if done { "☑" } else { "☐" },
                        entry.content
                    ))
                })),
        )
        .into_any_element(),
        ItemKind::Status(text) => div()
            .text_body_sm()
            .text_color(c.text_subtle)
            .child(text.clone())
            .into_any_element(),
    }
}

/// An agent reply as Markdown, like a message body.
fn reply_view<T: TranscriptHost>(
    this: &mut T,
    text: &str,
    key: &str,
    cx: &mut Context<T>,
) -> AnyElement {
    // Without the community, the reply shows as its plain text.
    let Some(community) = this.community(cx) else {
        return div().child(text.to_string()).into_any_element();
    };
    let community = community.read(cx);
    let directory = Directory::of(community, None, cx);
    let row = Arc::new(reply_row(text, key, community.session.viewer()));
    // Plain Markdown (`ActivityTranscript`): no plugin renderers, and web
    // links go to the browser.
    crate::ui::links::with_mode(crate::ui::links::LinkMode::Browser, || {
        crate::ui::renderers::without_renderers(|| markdown::body(&row, &directory, cx))
    })
}

fn tool_view<T: TranscriptHost>(
    this: &mut T,
    key: String,
    tool: &ToolItem,
    at: u64,
    cx: &mut Context<T>,
) -> AnyElement {
    let c = *cx.colors();
    let failed = tool.status == "failed";
    let name = Some(first_line(&tool.title))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "tool".into());
    // The first location, unless the title already names it.
    let path = tool
        .paths
        .first()
        .filter(|path| !tool.title.contains(path.as_str()))
        .cloned();
    let mono = |text: String, color: Hsla| {
        div()
            .min_w_0()
            .truncate()
            .font_family(font::MONO)
            .text_size(rems(0.8125))
            .text_color(color)
            .child(text)
    };
    let summary = div()
        .flex()
        .items_center()
        .gap(rems(0.5))
        .min_w_0()
        .text_color(if failed {
            c.text_danger
        } else {
            c.text_standard
        })
        .child(
            div()
                .flex_none()
                .child(Icon::new(tool_icon(&tool.kind)).size(16.).color(if failed {
                    c.text_danger
                } else {
                    c.text_standard
                })),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .gap(rems(0.25))
                .items_baseline()
                .when_some(tool_status(&tool.status), |this, status| {
                    this.child(
                        div()
                            .text_color(if failed { c.text_danger } else { c.text_subtle })
                            .child(status),
                    )
                })
                .child(mono(
                    name,
                    if failed {
                        c.text_danger
                    } else {
                        c.text_standard
                    },
                ))
                .when_some(path, |this, path| this.child(mono(path, c.text_subtle))),
        )
        .when_some(tool.permission.clone(), |this, permission| {
            this.child(
                div()
                    .id(SharedString::from(format!("{key}-permission")))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(rems(0.25))
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .tooltip(Tooltip::text(format!(
                        "Permission options: {}",
                        permission.options.join(", ")
                    )))
                    .child(Icon::new(IconName::Shield).size(14.).color(c.text_subtle))
                    .child(
                        permission
                            .outcome
                            .clone()
                            .unwrap_or_else(|| "Permission requested".into()),
                    ),
            )
        })
        .when_some(tool.completed_at, |this, done| {
            this.child(
                div()
                    .flex_none()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(duration(done.saturating_sub(at))),
            )
        })
        .into_any_element();
    let tool = tool.clone();
    disclosure(
        this,
        key,
        summary,
        move |_, cx| {
            let c = *cx.colors();
            let mut out = Vec::new();
            if tool.title.contains('\n') {
                out.push(code_block(&tool.title, cx));
            }
            if !tool.input.is_empty() {
                out.push(code_block(&tool.input, cx));
            }
            for diff in &tool.diffs {
                let (removed, added) = diff_hunk(diff);
                out.push(
                    div()
                        .mt(rems(0.25))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(format!(
                                    "{} {}",
                                    if diff.old_text.is_none() {
                                        "New file"
                                    } else {
                                        "Changed"
                                    },
                                    diff.path
                                )),
                        )
                        .child(
                            div()
                                .mt(rems(0.25))
                                .p(rems(0.5))
                                .rounded(radius::CONTROL)
                                .bg(c.surface_inset)
                                .text_mono()
                                .children(removed.into_iter().map(|line| {
                                    div().text_color(c.text_danger).child(format!("- {line}"))
                                }))
                                .children(added.into_iter().map(|line| {
                                    div().text_color(c.text_success).child(format!("+ {line}"))
                                })),
                        )
                        .into_any_element(),
                );
            }
            if !tool.output.is_empty() {
                out.push(code_block(&tool.output, cx));
            }
            if tool.truncated {
                out.push(
                    div()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child("Part of this tool call was too large and was elided by the agent.")
                        .into_any_element(),
                );
            }
            out
        },
        cx,
    )
}

/// A reply as a message row, so it renders like a message body.
fn reply_row(text: &str, key: &str, viewer: &str) -> MessageRow {
    use sha2_free::digest;
    let author = nostr::prelude::PublicKey::from_hex(viewer)
        .unwrap_or_else(|_| nostr::prelude::Keys::generate().public_key());
    MessageRow {
        id: nostr::prelude::EventId::from_byte_array(digest(key, text)),
        channel: Arc::from("agent-activity"),
        kind: 9,
        audience: None,
        author,
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
    }
}

/// A stable 32-byte id for a reply, from its key and text.
mod sha2_free {
    use std::hash::{Hash, Hasher};

    pub fn digest(key: &str, text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (round, chunk) in out.chunks_mut(8).enumerate() {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (round, key, text).hash(&mut hasher);
            chunk.copy_from_slice(&hasher.finish().to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn diffs_show_one_hunk() {
        let diff = ToolDiff {
            path: "a".into(),
            old_text: Some("a\nb\nc\n".into()),
            new_text: "a\nx\ny\nc\n".into(),
        };
        assert_eq!(
            diff_hunk(&diff),
            (vec!["b".into()], vec!["x".into(), "y".into()])
        );
        let new = ToolDiff {
            path: "n".into(),
            old_text: None,
            new_text: "one\n".into(),
        };
        assert_eq!(diff_hunk(&new), (vec![], vec!["one".into()]));
    }
}
