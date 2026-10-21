//! Hover cards on Buzz links (`BuzzLinkPreview.tsx`): a message link shows
//! its message, read exactly while the card is open; a channel link shows
//! the channel's public details. Clicking a card opens the link.

use super::markdown::Directory;
use super::{media, navigation, time};
use crate::mirror::mirror;
use buzz_relay_client::fold::MessageRow;
use buzz_relay_client::session::ThreadView;
use buzz_relay_client::{
    ChannelSummary, ChannelType, EventId, Session, TargetStatus, ThreadSnapshot, ThreadStatus,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Icon, IconName, MaterialStyled, Tooltip, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// The card's content width: `min(21.25rem, 100vw - 1.5rem)`.
const WIDTH: f32 = 340.;

/// `messagePreviewText`: stops before a blank paragraph rather than
/// clamping an empty line.
pub fn preview_text(content: &str) -> String {
    let text = content.trim();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len()
                && (bytes[j] == b'\n' || (bytes[j] == b'\r' && bytes.get(j + 1) == Some(&b'\n')))
            {
                let start = if i > 0 && bytes[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                };
                return format!("{}…", text[..start].trim_end());
            }
        }
        i += 1;
    }
    text.to_string()
}

/// `relativeTimestamp`: "just now", "5 minutes ago", …, "on Oct 2".
pub fn relative(seconds: u64, now: u64) -> String {
    let age = now.saturating_sub(seconds);
    let ago =
        |value: u64, unit: &str| format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" });
    match age {
        0..60 => "just now".into(),
        60..3600 => ago(age / 60, "minute"),
        3600..86400 => ago(age / 3600, "hour"),
        86400..604800 => ago(age / 86400, "day"),
        _ => format!("on {}", time::short_month_day(seconds)),
    }
}

/// An open message preview: the exact read holds while the card shows.
pub struct MessagePreview {
    message: EventId,
    snapshot: Arc<ThreadSnapshot>,
    _view: ThreadView,
    _watch: Task<()>,
}

impl MessagePreview {
    fn new(session: &Session, channel: &str, message: EventId, cx: &mut Context<Self>) -> Self {
        let view = session.open_thread(channel, message, true);
        view.refresh();
        let snapshot = view.snapshot.borrow().clone();
        let watch = mirror(view.snapshot.clone(), cx, |this: &mut Self, value| {
            this.snapshot = value;
        });
        Self {
            message,
            snapshot,
            _view: view,
            _watch: watch,
        }
    }

    fn message(&self) -> Option<&Arc<MessageRow>> {
        let snapshot = &self.snapshot;
        snapshot
            .target
            .as_ref()
            .filter(|t| t.id == self.message)
            .or_else(|| {
                snapshot
                    .root
                    .iter()
                    .chain(snapshot.replies.iter())
                    .find(|row| row.id == self.message)
            })
    }

    /// Only reconciled content paints; a terminal state says so rather
    /// than loading forever.
    fn state(&self) -> Result<Option<&Arc<MessageRow>>, ()> {
        let snapshot = &self.snapshot;
        let message = self.message();
        let is_target = snapshot
            .target
            .as_ref()
            .is_some_and(|t| t.id == self.message);
        let stopped = snapshot.status == ThreadStatus::Error
            || matches!(
                snapshot.target_status,
                Some(TargetStatus::Error | TargetStatus::Unavailable)
            )
            || (snapshot.status == ThreadStatus::Idle && snapshot.error.is_some())
            || (snapshot.status == ThreadStatus::Ready
                && message.is_none()
                && !snapshot.can_load_more);
        let ready = if is_target {
            snapshot.target_status == Some(TargetStatus::Ready)
        } else {
            snapshot.status == ThreadStatus::Ready
        };
        match (ready, message) {
            (true, Some(message)) => Ok(Some(message)),
            _ if stopped => Err(()),
            _ => Ok(None),
        }
    }
}

fn card_frame(id: SharedString, url: &str, cx: &App) -> Stateful<Div> {
    let c = *cx.colors();
    let target = url.to_string();
    // A reader's card opens nothing (`onOpenLink` answers false).
    let closed = super::links::mode() == super::links::LinkMode::Closed;
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(8.))
        .w(px(WIDTH))
        .max_h(px(320.))
        .overflow_hidden()
        .p(px(12.))
        .floating_surface(&c)
        .rounded(px(16.))
        .text_body()
        .text_color(c.text_standard)
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            if !closed {
                navigation::open_url(&target, window, cx)
            }
        })
}

/// The card for a message link, opening the exact read the first time it
/// shows.
#[allow(clippy::too_many_arguments)]
pub fn message_card(
    id: SharedString,
    session: Arc<Session>,
    channel: &str,
    message: EventId,
    directory: &Directory,
    url: &str,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let channel_id = channel.to_string();
    let preview = window.use_keyed_state(id.clone(), cx, move |_, cx| {
        MessagePreview::new(&session, &channel_id, message, cx)
    });
    let c = *cx.colors();
    let status = |text: &'static str| {
        div()
            .text_body_sm()
            .text_color(c.text_subtle)
            .child(text)
            .into_any_element()
    };
    let preview = preview.read(cx);
    let body = match preview.state() {
        Err(()) => status("Message preview unavailable."),
        Ok(None) => status("Loading message…"),
        Ok(Some(row)) => message_body(row, directory, cx),
    };
    card_frame(SharedString::from(format!("{id}-frame")), url, cx)
        .child(body)
        .into_any_element()
}

fn message_body(row: &Arc<MessageRow>, directory: &Directory, cx: &App) -> AnyElement {
    let c = *cx.colors();
    let names = &directory.names;
    let display = row.workflow_owner.unwrap_or(row.author).to_hex();
    let resolved = names.display(&display);
    let name = match row.workflow_owner {
        Some(_) => format!("Workflow · owned by {resolved}"),
        None => resolved,
    };
    let profile = directory.profiles.get(&display);
    let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Default);
    if row.workflow_owner.is_some() {
        avatar = avatar.squircle().fallback_icon(IconName::Lightning, 20.);
    } else if let Some(picture) = profile.and_then(|p| p.picture.as_deref()) {
        avatar = avatar.image(media::avatar(picture, &directory.media));
    }
    directory.want_profiles(vec![display.clone()]);
    let channel = directory.channels.iter().find(|c| c.id == *row.channel);
    let channel_name = match channel {
        Some(channel) if channel.channel_type == Some(ChannelType::Dm) => {
            match &channel.participants {
                Some(people) => {
                    let joined = people
                        .iter()
                        .map(|id| names.display(id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if joined.is_empty() {
                        "Notes to self".to_string()
                    } else {
                        joined
                    }
                }
                None => channel.name.clone(),
            }
        }
        Some(channel) => channel.name.clone(),
        None => "Channel unavailable".to_string(),
    };
    let icon = channel.map(|channel| {
        if channel.hidden || channel.channel_type == Some(ChannelType::Dm) || channel.private {
            IconName::Lock
        } else {
            IconName::Hash
        }
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let text = preview_text(&row.content);
    let text = if text.is_empty() {
        if row.attachments.is_empty() {
            "Empty message".to_string()
        } else {
            "Attachment".to_string()
        }
    } else {
        text
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div().flex().items_center().gap(px(8.)).child(avatar).child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(4.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .min_w_0()
                                    .truncate()
                                    .child(name),
                            )
                            .child(
                                div()
                                    .id("preview-time")
                                    .flex_none()
                                    .text_body_sm()
                                    .text_color(c.text_subtle)
                                    .tooltip(Tooltip::text(time::full_short(row.created_at)))
                                    .child(relative(row.created_at, now)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .mt(px(-2.))
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .children(icon.map(|icon| Icon::new(icon).size(13.)))
                            .child(div().min_w_0().truncate().child(channel_name)),
                    ),
            ),
        )
        .child(
            div()
                .max_h(px(4. * 24.))
                .overflow_hidden()
                .text_ellipsis()
                .line_clamp(4)
                .child(text),
        )
        .into_any_element()
}

/// The card for a channel-only link: name, "Public/Private channel ·
/// Archived · Joined/Not joined · N members", then its description.
pub fn channel_card(channel: &ChannelSummary, joined: bool, url: &str, cx: &App) -> AnyElement {
    let c = *cx.colors();
    let members = channel.members.len();
    let details: Vec<String> = [
        Some(
            if channel.private {
                "Private channel"
            } else {
                "Public channel"
            }
            .to_string(),
        ),
        channel.archived.then(|| "Archived".to_string()),
        Some(if joined { "Joined" } else { "Not joined" }.to_string()),
        (members > 0).then(|| {
            format!(
                "{members} {}",
                if members == 1 { "member" } else { "members" }
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    card_frame(
        SharedString::from(format!("channel-card-{}", channel.id)),
        url,
        cx,
    )
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                Icon::new(if channel.private {
                    IconName::Lock
                } else {
                    IconName::Hash
                })
                .size(20.),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(channel.name.clone()),
                    )
                    .child(
                        div()
                            .mt(px(-2.))
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .truncate()
                            .child(details.join(" · ")),
                    ),
            ),
    )
    .when_some(
        channel.description.clone().filter(|d| !d.is_empty()),
        |d, description| d.child(div().line_clamp(4).child(description)),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn excerpts_stop_at_a_blank_paragraph() {
        assert_eq!(preview_text("  one\ntwo  "), "one\ntwo");
        assert_eq!(preview_text("one\n\ntwo"), "one…");
        assert_eq!(preview_text("one  \n  \r\ntwo"), "one…");
    }

    #[test]
    fn relative_times_follow_the_ladder() {
        assert_eq!(relative(1000, 1030), "just now");
        assert_eq!(relative(1000, 1000 + 60), "1 minute ago");
        assert_eq!(relative(1000, 1000 + 7200), "2 hours ago");
        assert_eq!(relative(1000, 1000 + 86400 * 3), "3 days ago");
    }
}
