//! "Latest activity" on an agent's profile (`ProfileActivity.tsx`): the
//! last few reply and tool lines in the open conversation, read from the
//! captured activity. Showing a profile never starts telemetry.

use crate::agent_activity::current::activity_preview;
use crate::agent_activity::{FeedStatus, observer};
use crate::community::Community;
use buzz_ui::tokens::radius;
use buzz_ui::{ActiveTheme, Button, ButtonSize, TypeStyled};
use gpui_kit::*;

/// `Oct 10, 3:08 AM`.
fn short_time(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|time| time.format("%b %-d, %-I:%M %p").to_string())
        .unwrap_or_default()
}

/// The section for `pubkey` in `channel`, or `None` when it does not
/// apply: no conversation context, not an agent, or activity disabled.
pub fn latest_activity(
    community: &Entity<Community>,
    pubkey: &str,
    channel: Option<&str>,
    on_view: Option<super::row::Action>,
    cx: &App,
) -> Option<AnyElement> {
    let channel = channel?.to_string();
    let snapshot = observer::snapshot(community, cx);
    if snapshot.status == FeedStatus::Disabled {
        return None;
    }
    {
        let community = community.read(cx);
        let known = community
            .snapshot
            .profiles
            .get(pubkey)
            .is_some_and(|profile| profile.is_agent)
            || super::owned_agents(community, cx).contains(pubkey);
        if !known {
            return None;
        }
    }
    let c = *cx.colors();
    let items = activity_preview(&snapshot.records, pubkey, &channel);
    // Another identity's records never count; nor do other channels.
    let latest = snapshot
        .turns
        .iter()
        .filter(|turn| turn.agent == pubkey && turn.channel_id.as_deref() == Some(&channel))
        .map(|turn| turn.timestamp)
        .chain(items.iter().map(|item| item.timestamp))
        .max();
    let status = match snapshot.status {
        FeedStatus::Unavailable => "Activity unavailable".to_string(),
        FeedStatus::Connecting => "Connecting…".to_string(),
        FeedStatus::Interrupted => "Activity disconnected".to_string(),
        _ => latest.map_or("No activity yet".to_string(), short_time),
    };
    let body: Vec<AnyElement> = if items.is_empty() {
        vec![
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Live activity will appear here.")
                .into_any_element(),
        ]
    } else {
        items
            .into_iter()
            .map(|item| {
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child(item.label),
                    )
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_standard)
                            .line_clamp(3)
                            .child(item.text),
                    )
                    .into_any_element()
            })
            .collect()
    };
    let agent = pubkey.to_string();
    Some(
        div()
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .min_w_0()
            .mt(rems(1.))
            .p(rems(1.))
            .border_1()
            .border_color(c.border_standard)
            .rounded(radius::CONTAINER)
            .bg(c.surface_inset)
            .child(div().text_body().child("Latest activity"))
            .child(div().flex().flex_col().gap(rems(0.75)).children(body))
            .child(div().text_body_sm().text_color(c.text_subtle).child(status))
            .child(
                div().flex().child(
                    Button::new("profile-view-activity", "View activity")
                        .size(ButtonSize::Sm)
                        // The profile's own Activity tab when it has one.
                        .on_click(move |_, window, cx| match &on_view {
                            Some(view) => view(window, cx),
                            None => super::open(&agent, Some(&channel), None, window, cx),
                        }),
                ),
            )
            .into_any_element(),
    )
}
