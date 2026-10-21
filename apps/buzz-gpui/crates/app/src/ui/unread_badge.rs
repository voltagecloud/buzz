//! A sidebar row's unread state (`UnreadBadge.tsx`): the label goes bold
//! for ordinary backlog, a 6px accent dot marks attention, and another
//! marks unread threads. There are no numerals: counts are sampled, not
//! exact (ui.md §3).

use buzz_relay_client::{ChannelUnread, Manual};
use buzz_ui::{ActiveTheme, Tooltip};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// What the badge says to assistive technology.
pub fn label(unread: &ChannelUnread) -> String {
    match unread.manual {
        Manual::LocalOnly => "Marked unread on this device only".into(),
        Manual::Remote => "Marked unread".into(),
        Manual::None => format!(
            "{} observed unread messages{}. Not an exact total.",
            unread.count,
            if unread.stale {
                "; may be out of date"
            } else {
                ""
            }
        ),
    }
}

/// The thread dot's tooltip: "2 unread threads; may be out of date".
pub fn thread_label(unread: &ChannelUnread) -> String {
    format!(
        "{} unread {}{}",
        unread.threads,
        if unread.threads == 1 {
            "thread"
        } else {
            "threads"
        },
        if unread.threads_stale {
            "; may be out of date"
        } else {
            ""
        }
    )
}

/// The dot a row shows, if any: unread threads take the place of the
/// attention dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Priority,
    Threads,
}

pub fn dot(unread: &ChannelUnread, dm: bool) -> Option<Dot> {
    if unread.threads > 0 {
        Some(Dot::Threads)
    } else if unread.unread && (dm || unread.attention > 0) {
        Some(Dot::Priority)
    } else {
        None
    }
}

/// The row's trailing badge, or `None` when nothing is unread.
pub fn trailing(id: &str, unread: &ChannelUnread, dm: bool, cx: &App) -> Option<AnyElement> {
    let dot = dot(unread, dm);
    if !unread.unread && dot.is_none() {
        return None;
    }
    let c = cx.colors();
    let threads = thread_label(unread);
    Some(
        div()
            .id(SharedString::from(format!("unread-{id}")))
            .flex()
            .items_center()
            .ml_auto()
            .when(unread.unread, |d| d.aria_label(label(unread)))
            .when_some(dot, |d, dot| {
                d.child(
                    div()
                        .id(SharedString::from(format!("unread-dot-{id}")))
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(c.affordance_prominent)
                        .when(dot == Dot::Threads, |d| {
                            d.aria_label(threads.clone())
                                .tooltip(Tooltip::text(threads.clone()))
                        }),
                )
            })
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn unread() -> ChannelUnread {
        ChannelUnread {
            unread: true,
            count: 3,
            ..Default::default()
        }
    }

    #[test]
    fn labels_follow_the_reference() {
        assert_eq!(
            label(&unread()),
            "3 observed unread messages. Not an exact total."
        );
        let stale = ChannelUnread {
            stale: true,
            ..unread()
        };
        assert_eq!(
            label(&stale),
            "3 observed unread messages; may be out of date. Not an exact total."
        );
        let local = ChannelUnread {
            manual: Manual::LocalOnly,
            ..unread()
        };
        assert_eq!(label(&local), "Marked unread on this device only");
        let threads = ChannelUnread {
            threads: 1,
            ..unread()
        };
        assert_eq!(thread_label(&threads), "1 unread thread");
        let more = ChannelUnread {
            threads: 2,
            threads_stale: true,
            ..unread()
        };
        assert_eq!(thread_label(&more), "2 unread threads; may be out of date");
    }

    #[test]
    fn attention_shows_a_dot_and_threads_take_its_place() {
        assert_eq!(dot(&unread(), false), None);
        let mention = ChannelUnread {
            attention: 1,
            ..unread()
        };
        assert_eq!(dot(&mention, false), Some(Dot::Priority));
        assert_eq!(dot(&unread(), true), Some(Dot::Priority));
        let threads = ChannelUnread {
            threads: 2,
            ..mention
        };
        assert_eq!(dot(&threads, false), Some(Dot::Threads));
        let read = ChannelUnread::default();
        assert_eq!(dot(&read, true), None);
    }
}
