//! What the page shows when a destination isn't open (`App.tsx`,
//! `ChannelsPage.tsx`, `ChannelSidebar.tsx`, `navigation/controller.ts`):
//! - "Opening destination…" while a place waits for its channel list;
//! - "Checking conversation access…" while an unjoined channel is looked
//!   up;
//! - "This destination couldn’t open", with the reason, Retry navigation
//!   and Open Settings;
//! - the "Your channels, one conversation." splash without a connected
//!   community, and the personal space's sidebar;
//! - the empty selection.

use super::navigation::Target;
use buzz_relay_client::{ChannelSummary, ChannelType, SessionSnapshot, SessionStatus};
use buzz_ui::{ActiveTheme, Button, ButtonSize, Icon, IconName, TypeStyled};
use gpui_kit::*;
use std::time::Duration;

/// A navigation that still waits after this fails as unavailable
/// (`controller.ts`, the 15 s open timeout).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// What fills the Messages page without an open channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Empty {
    /// No community is selected: the personal space.
    NoCommunity,
    /// The community is not connected and nothing is saved to show.
    Splash,
    /// A place waits for the channel list (`route.waiting`).
    Opening,
    /// A routed channel is being looked up.
    Checking,
    /// Nothing is selected.
    Select,
}

/// The splash shows while the session is not ready and has no channels
/// to show (`session.status !== "ready" && !session.cached`).
pub fn empty(snapshot: Option<&SessionSnapshot>, opening: bool, checking: bool) -> Empty {
    if checking {
        return Empty::Checking;
    }
    if opening {
        return Empty::Opening;
    }
    match snapshot {
        None => Empty::NoCommunity,
        Some(snapshot)
            if snapshot.status != SessionStatus::Ready && snapshot.channels.is_empty() =>
        {
            Empty::Splash
        }
        Some(_) => Empty::Select,
    }
}

/// The empty page: "Your channels, one conversation." without a
/// connected community, else a status line in the page's corner; `select`
/// is what the page says with nothing selected.
pub fn render(empty: Empty, select: &'static str, cx: &App) -> AnyElement {
    let c = cx.colors();
    match empty {
        Empty::NoCommunity | Empty::Splash => div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .p(px(32.))
            .bg(c.surface_panel)
            .text_center()
            .child(
                div()
                    .p(px(20.))
                    .mb(px(16.))
                    .rounded(px(20.))
                    .bg(c.affordance_subtle)
                    .text_color(c.text_standard)
                    .child(Icon::new(IconName::Plug).size(30.)),
            )
            .child(
                div()
                    .text_title()
                    .text_color(c.text_standard)
                    .child("Your channels, one conversation."),
            )
            .child(
                div()
                    .max_w(px(550.))
                    .mt(px(8.))
                    .text_body()
                    .text_color(c.text_subtle)
                    .child(if empty == Empty::NoCommunity {
                        "Use the left community rail to choose or add a community. Your profile \
                         and settings work without a community."
                    } else {
                        "Connection details and retry are in the sidebar. Your profile and \
                         settings work without a community."
                    }),
            )
            .into_any_element(),
        Empty::Opening | Empty::Checking | Empty::Select => div()
            .flex_1()
            .py(px(32.))
            .px(px(16.))
            .text_body_sm()
            .text_color(c.text_subtle)
            .child(match empty {
                Empty::Opening => "Opening destination…",
                Empty::Checking => "Checking conversation access…",
                _ => select,
            })
            .into_any_element(),
    }
}

/// Why a destination could not open (`OpenFailure`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The target needs another account or a community not joined here.
    Denied,
    /// An outside link that cannot be parsed.
    Invalid,
    /// The conversation is not there (`not-found`).
    NotFound,
    /// It took too long to open (`timeout`).
    Timeout,
    /// The relay could not be asked (`host-error`).
    HostError,
    /// The page that shows it says it failed (`PageNavigation.complete`);
    /// `unavailable` to plugins.
    Page,
    /// An outside link held until there is an identity and a community
    /// to bind it to, opened by Retry navigation (`deep-links.ts`
    /// `unavailable`).
    Held,
}

/// A navigation that could not open, kept for Retry navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct Failed {
    pub target: Option<Target>,
    pub reason: Reason,
    /// It came from outside the window: a link from the OS, another
    /// launch or a notification.
    pub ingress: bool,
}

impl Failed {
    /// Outside links retry only when they may succeed later.
    pub fn retryable(&self) -> bool {
        !self.ingress
            || self.reason == Reason::Held
            || (self.reason != Reason::Invalid && self.target.is_some())
    }

    /// The notice's explanation.
    pub fn explanation(&self) -> &'static str {
        match self.reason {
            Reason::Denied => {
                "This target needs its original account and an already joined community."
            }
            _ if self.ingress && !self.retryable() => "This link is invalid or unsupported.",
            _ => {
                "The destination is unavailable or isn’t supported yet. Your target has been \
                 kept for retry."
            }
        }
    }
}

/// "This destination couldn’t open" (`App.tsx`), with Retry navigation
/// when the target may still open, and Open Settings.
pub fn failure(
    failed: &Failed,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_settings: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let c = cx.colors();
    div()
        .flex_1()
        .p(px(20.))
        .child(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(12.))
                .my(px(20.))
                .p(px(20.))
                .rounded(px(16.))
                .border_1()
                .border_color(c.border_warning)
                .bg(c.affordance_warning)
                .text_body_sm()
                .text_color(c.text_warning)
                .child(div().text_heading().child("This destination couldn’t open"))
                .child(failed.explanation())
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .children(failed.retryable().then(|| {
                            Button::new("retry-navigation", "Retry navigation")
                                .size(ButtonSize::Md)
                                .on_click(on_retry)
                        }))
                        .child(
                            Button::new("failure-settings", "Open Settings")
                                .size(ButtonSize::Md)
                                .on_click(on_settings),
                        ),
                ),
        )
        .into_any_element()
}

/// The channel a community opens on without a remembered one: the first
/// listed that is not an agent session (`ChannelsPage`'s default).
pub fn default_channel(snapshot: &SessionSnapshot) -> Option<ChannelSummary> {
    snapshot
        .channels
        .iter()
        .find(|channel| {
            channel.channel_type != Some(ChannelType::Session)
                && !channel.archived
                && !snapshot.hidden_dms.contains(&channel.id)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn the_default_channel_skips_sessions() {
        let channel = |id: &str, kind| ChannelSummary {
            id: id.into(),
            channel_type: Some(kind),
            ..Default::default()
        };
        let channels = vec![
            channel("s", ChannelType::Session),
            channel("g", ChannelType::Stream),
        ];
        let snapshot = SessionSnapshot {
            channels: channels.into(),
            ..test_snapshot()
        };
        assert_eq!(default_channel(&snapshot).unwrap().id, "g");
        // Saved channels still show while connecting.
        assert_eq!(empty(Some(&snapshot), false, false), Empty::Select);
        assert_eq!(empty(Some(&snapshot), true, false), Empty::Opening);
        assert_eq!(empty(Some(&snapshot), true, true), Empty::Checking);
        let offline = test_snapshot();
        assert_eq!(empty(Some(&offline), false, false), Empty::Splash);
        let ready = SessionSnapshot {
            status: SessionStatus::Ready,
            ..offline
        };
        assert_eq!(empty(Some(&ready), false, false), Empty::Select);
        assert_eq!(empty(None, false, false), Empty::NoCommunity);
    }

    fn failed(reason: Reason, ingress: bool, target: bool) -> Failed {
        Failed {
            target: target.then(|| Target::channel(None, "general")),
            reason,
            ingress,
        }
    }

    #[test]
    fn failure_copy_follows_the_reason() {
        let denied = failed(Reason::Denied, false, true);
        assert!(denied.explanation().starts_with("This target needs"));
        assert!(denied.retryable());
        let invalid = failed(Reason::Invalid, true, false);
        assert_eq!(
            invalid.explanation(),
            "This link is invalid or unsupported."
        );
        assert!(!invalid.retryable());
        let kept = failed(Reason::Timeout, true, true);
        assert!(kept.explanation().ends_with("kept for retry."));
        assert!(kept.retryable());
    }

    fn test_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            status: SessionStatus::Connecting,
            live: false,
            cached: false,
            restoring: false,
            live_status: Default::default(),
            relay: None,
            roster: buzz_relay_client::RosterStatus::Loading,
            channels: std::sync::Arc::new([]),
            profiles: Default::default(),
            unread: Default::default(),
            presence: Default::default(),
            typing: Default::default(),
            thread_typing: Default::default(),
            typing_anywhere: Default::default(),
            sidebar: Default::default(),
            hidden_dms: std::sync::Arc::new([]),
            dm_visibility: Default::default(),
            emoji: Default::default(),
            statuses: Default::default(),
        }
    }
}
