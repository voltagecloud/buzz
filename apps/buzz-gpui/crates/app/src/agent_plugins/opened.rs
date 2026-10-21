//! The session of the conversation an agent's profile was opened from,
//! set apart above the agent's other conversations (buzz-app's
//! `OpenedConversation.tsx`), and the transcript a type's tab shows over
//! itself. Types list and find their own sessions.

use crate::agent_plugins::transcript_view::Back;
use crate::agents2::AgentConversation;
use crate::ui::agent_activity::panel::date_time;
use buzz_ui::tokens::{radius, space};
use buzz_ui::{Button, Colors, IconButton, IconButtonSize, IconName, TypeStyled};
use gpui_kit::*;

/// Conversations listed below the one the profile was opened from.
pub const CONVERSATION_LIMIT: usize = 30;

/// A conversation's channel, and its thread root when it is a thread's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub channel_id: String,
    pub root: Option<String>,
}

/// Where to look for `conversation`'s session: its thread's, then its
/// channel's, which is the one session a channel-scoped agent or a DM
/// keeps.
pub fn places(conversation: &AgentConversation) -> Vec<Place> {
    let channel = Place {
        channel_id: conversation.channel_id.clone(),
        root: None,
    };
    match &conversation.root_id {
        Some(root) => vec![
            Place {
                channel_id: conversation.channel_id.clone(),
                root: Some(root.clone()),
            },
            channel,
        ],
        None => vec![channel],
    }
}

/// What a lookup for a session another copy of the app started found:
/// `Ok(None)` when there is none.
pub type Found<T> = Result<Option<T>, String>;

/// The opened conversation's session as the card shows it.
pub struct CardSession {
    pub channel_id: String,
    pub name: Option<String>,
    pub root: Option<String>,
    pub at: Option<u64>,
    /// The session's live state, when it is running.
    pub status: Option<String>,
}

/// The "This thread" / "This channel" card. `found` is `None` while the
/// type still looks for a session.
pub fn card(
    id: &str,
    conversation: &AgentConversation,
    harness: &str,
    found: Option<Found<CardSession>>,
    on_view: impl Fn(&mut Window, &mut App) + 'static,
    c: &Colors,
) -> Div {
    let thread = match &found {
        Some(Ok(Some(session))) => session.root.is_some(),
        _ => conversation.root_id.is_some(),
    };
    let place = if thread { "thread" } else { "channel" };
    let body = match found {
        Some(Ok(Some(session))) => {
            let title = match &session.name {
                Some(name) => format!("#{name}"),
                None => session.channel_id.chars().take(8).collect(),
            };
            let detail: Vec<String> = session
                .status
                .into_iter()
                .chain(
                    session
                        .at
                        .map(|at| format!("Last active {}", date_time(at))),
                )
                .collect();
            let detail = if detail.is_empty() {
                format!("{harness} session")
            } else {
                detail.join(" · ")
            };
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(space::S3)
                .w_full()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().truncate().text_label().child(title))
                        .child(div().text_body_sm().text_color(c.text_subtle).child(detail)),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-view")), "View transcript")
                        .prominent()
                        .on_click(move |_, window, cx| on_view(window, cx)),
                )
        }
        None => div()
            .text_body_sm()
            .text_color(c.text_subtle)
            .child(format!("Looking for a {harness} session…")),
        Some(Err(error)) => div()
            .text_body_sm()
            .child(format!("Could not look for a {harness} session: {error}")),
        Some(Ok(None)) => div()
            .flex()
            .flex_col()
            .gap(space::S1)
            .child(
                div()
                    .text_label()
                    .child(format!("No {harness} session here yet")),
            )
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(format!(
                        "One starts when you mention the agent in this {place}."
                    )),
            ),
    };
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .child(heading(&format!("This {place}")))
        .child(
            div()
                .flex()
                .rounded(radius::CONTAINER)
                .bg(c.surface_inset)
                .p(space::S4)
                .child(body),
        )
}

/// A section's inline header.
pub fn heading(title: &str) -> Div {
    div().font_weight(FontWeight::BOLD).child(title.to_string())
}

/// A transcript over its tab (`PanelSubview`): back to the profile, the
/// conversation's title, then the transcript. Escape goes back too.
pub fn subview(
    title: String,
    focus: &FocusHandle,
    on_back: Back,
    content: impl IntoElement,
) -> Stateful<Div> {
    let (back, escape) = (on_back.clone(), on_back);
    div()
        .id("agent-transcript")
        .track_focus(focus)
        .flex()
        .flex_col()
        .gap(space::S4)
        .min_w_0()
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape(window, cx);
            }
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S2)
                .min_w_0()
                .child(
                    IconButton::new("agent-transcript-back", IconName::ArrowLeft)
                        .size(IconButtonSize::Sm)
                        .icon_size(18.)
                        .tooltip("Back to profile")
                        .on_click(move |_, window, cx| back(window, cx)),
                )
                .child(div().truncate().text_label().child(title)),
        )
        .child(content)
}
