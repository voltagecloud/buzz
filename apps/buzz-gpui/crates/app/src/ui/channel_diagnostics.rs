//! The tools in channel settings' "Diagnostics" (`ChannelsPage.tsx`'s
//! settings children) above the outbox: the unread options
//! (`UnreadOptions`), live updates (`LiveStatus` with `diagnostics`), the
//! roster and the refresh actions.

use crate::community::Community;
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_relay_client::{
    CatchUpState, LiveConnection, LiveRoute, LiveRouteStatus, LiveView, ReadCapability,
    ReadSyncStatus, RosterRefresh,
};
use buzz_ui::{ActiveTheme, Button, Icon, IconName, TypeStyled, tokens::space};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// `ReadCapability` as buzz-app names it.
pub fn capability_label(capability: ReadCapability) -> &'static str {
    match capability {
        ReadCapability::Unsupported => "unsupported",
        ReadCapability::ReadOnly => "read-only",
        ReadCapability::FrontierSync => "frontier-sync",
    }
}

pub fn sync_status_label(status: ReadSyncStatus) -> &'static str {
    match status {
        ReadSyncStatus::Loading => "loading",
        ReadSyncStatus::Local => "local",
        ReadSyncStatus::Pending => "pending",
        ReadSyncStatus::Accepted => "accepted",
        ReadSyncStatus::Reconciled => "reconciled",
        ReadSyncStatus::Stale => "stale",
        ReadSyncStatus::Error => "error",
    }
}

/// The "Live updates: …" line and the "Last rejection: …" one, as
/// `LiveStatus` reads them with `diagnostics`.
pub fn live_lines(view: &LiveView, channel: Option<&str>) -> (String, Option<String>) {
    let route = channel.and_then(|id| {
        view.live
            .routes
            .iter()
            .find(|route| route.channel.as_deref() == Some(id))
    });
    let globals: Vec<&LiveRoute> = view
        .live
        .routes
        .iter()
        .filter(|route| route.channel.is_none())
        .collect();
    let relevant: Vec<&LiveRoute> = route.into_iter().chain(globals.iter().copied()).collect();
    let recovering: Vec<&LiveRoute> = relevant
        .iter()
        .copied()
        .filter(|route| {
            view.live.status == LiveConnection::Connected
                && route.status == LiveRouteStatus::Pending
                && route
                    .error
                    .as_deref()
                    .and_then(buzz_relay_client::socket::quota_hint)
                    .is_some_and(|seconds| seconds <= 60)
        })
        .collect();
    let head = channel.and_then(|id| view.heads.iter().find(|head| head.channel == id));
    let roster_error = match &view.roster {
        RosterRefresh::Error(error) => Some(error.clone()),
        _ => None,
    };
    let error = view
        .live
        .error
        .clone()
        .or(roster_error)
        .or_else(|| head.and_then(|head| head.error.clone()))
        .or_else(|| {
            relevant
                .iter()
                .filter(|route| !recovering.iter().any(|r| r.id == route.id))
                .find_map(|route| route.error.clone())
        });
    let state = match view.issue(channel) {
        Some(reason) => reason,
        None if !recovering.is_empty() => {
            "recovering automatically after rate limiting; awaiting confirmation".into()
        }
        None if view.live.status == LiveConnection::Connecting
            || (channel.is_some() && route.is_none_or(|r| r.status != LiveRouteStatus::Live))
            || globals
                .iter()
                .any(|route| route.status == LiveRouteStatus::Pending)
            || head.is_some_and(|head| head.state == CatchUpState::Pending) =>
        {
            "connecting".into()
        }
        None => "stream established".into(),
    };
    let rejection = error.or_else(|| recovering.first().and_then(|route| route.error.clone()));
    (format!("Live updates: {state}"), rejection)
}

/// The tools, kept by the settings panel for its channel.
pub struct ChannelDiagnostics {
    community: Entity<Community>,
    channel: String,
    unread_open: bool,
    /// The last unread option's failure.
    error: Option<SharedString>,
    _observe: Subscription,
}

impl ChannelDiagnostics {
    pub fn new(community: Entity<Community>, channel: String, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&community, |_, _, cx| cx.notify());
        Self {
            community,
            channel,
            unread_open: false,
            error: None,
            _observe: observe,
        }
    }

    fn session(&self, cx: &App) -> Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    /// Runs an unread option; its failure shows under the buttons.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        operation: impl std::future::Future<Output = Result<(), String>> + 'static,
    ) {
        self.error = None;
        cx.spawn(async move |this, cx| {
            if let Err(error) = operation.await {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(error.into());
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn mark_unread(&mut self, cx: &mut Context<Self>) {
        let (session, channel) = (self.session(cx), self.channel.clone());
        self.run(
            cx,
            async move { session.mark_channel_unread(&channel).await },
        );
    }

    /// "Mark read through loaded messages": through the newest loaded row
    /// that is not a membership notice.
    fn mark_through_loaded(&mut self, cx: &mut Context<Self>) {
        let session = self.session(cx);
        let view = session.open_channel(&self.channel);
        let last = view
            .snapshot
            .borrow()
            .rows
            .iter()
            .rev()
            .find(|row| row.membership.is_none())
            .map(|row| row.id);
        drop(view);
        let Some(last) = last else {
            self.error = Some("Load a verified message before marking through it.".into());
            cx.notify();
            return;
        };
        let channel = self.channel.clone();
        self.run(cx, async move {
            session.mark_through(&channel, None, last).await
        });
    }

    /// A `<details>` summary: a caret and the label.
    fn summary(
        id: &'static str,
        label: SharedString,
        open: bool,
        cx: &App,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let c = cx.colors();
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(space::S1)
            .cursor_pointer()
            .text_color(c.text_subtle)
            .child(
                Icon::new(if open {
                    IconName::CaretDown
                } else {
                    IconName::CaretRight
                })
                .size(12.),
            )
            .child(label)
            .on_click(on_click)
    }

    fn unread_options(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = *cx.colors();
        let snapshot = self.community.read(cx).snapshot.clone();
        let sync = snapshot.unread.sync.clone();
        let frontier = sync
            .as_ref()
            .is_some_and(|sync| sync.capability == ReadCapability::FrontierSync);
        let mut items = vec![
            div()
                .child(
                    Button::new("diagnostics-mark-unread", "Mark unread on this device")
                        .on_click(cx.listener(|this, _, _, cx| this.mark_unread(cx))),
                )
                .into_any_element(),
        ];
        if frontier {
            items.push(
                div()
                    .child(
                        Button::new(
                            "diagnostics-mark-through",
                            "Mark read through loaded messages",
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.mark_through_loaded(cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(error) = &self.error {
            items.push(
                div()
                    .text_color(c.text_danger)
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        let weak = cx.entity().downgrade();
        items.push(
            Self::summary(
                "diagnostics-unread-status",
                "Unread status".into(),
                self.unread_open,
                cx,
                move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.unread_open = !this.unread_open;
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
        );
        if self.unread_open {
            let (capability, status, error) = match &sync {
                Some(sync) => (
                    capability_label(sync.capability),
                    sync_status_label(sync.status),
                    sync.error.clone(),
                ),
                None => ("unsupported", "loading", None),
            };
            items.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S3)
                    .child(
                        "Observed messages only—not exact totals. Manual unread is local to \
                         this device. Older read hints can expire; this is not an everlasting \
                         read receipt log.",
                    )
                    .child(format!("Read sync: {capability} · {status}"))
                    .children(error.map(|error| div().text_color(c.text_danger).child(error)))
                    .child(
                        div().child(
                            Button::new(
                                "diagnostics-refresh-unread",
                                "Refresh unread observations",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error = None;
                                this.session(cx).refresh_unread();
                                cx.notify();
                            })),
                        ),
                    )
                    .when(frontier, |this| {
                        this.child(div().child(
                            Button::new("diagnostics-retry-sync", "Retry read sync").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    this.session(cx).retry_read_sync();
                                    cx.notify();
                                }),
                            ),
                        ))
                    })
                    .into_any_element(),
            );
        }
        items
    }
}

impl Render for ChannelDiagnostics {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.community.read(cx).snapshot.clone();
        let (live, rejection) = live_lines(&snapshot.live_status, Some(&self.channel));
        let preferences_error = match &snapshot.sidebar.status {
            PreferencesStatus::Error(error) => Some(error.clone()),
            _ => None,
        };
        let loading = snapshot.sidebar.status == PreferencesStatus::Loading;
        div()
            .flex()
            .flex_col()
            .gap(space::S3)
            .text_body_sm()
            .children(self.unread_options(cx))
            .child(live)
            .children(rejection.map(|error| format!("Last rejection: {error}")))
            .child(format!("Roster · {} channels", snapshot.channels.len()))
            .child(
                div().child(
                    Button::new("diagnostics-refresh-channels", "Refresh channels")
                        .on_click(cx.listener(|this, _, _, cx| this.session(cx).visible())),
                ),
            )
            .children(preferences_error.map(|error| format!("Saved groups and stars: {error}")))
            .child(
                div().child(
                    Button::new(
                        "diagnostics-refresh-preferences",
                        "Refresh groups and stars",
                    )
                    .disabled(loading)
                    .on_click(cx.listener(|this, _, _, cx| this.session(cx).refresh_sidebar())),
                ),
            )
            .child(div().child(
                Button::new("diagnostics-refresh-messages", "Refresh messages").on_click(
                    cx.listener(|this, _, _, cx| {
                        let channel = this.channel.clone();
                        this.session(cx).refresh_channel(&channel)
                    }),
                ),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::{HeadCatchUp, LiveSnapshot, Replay};
    use core::prelude::v1::test;

    fn route(channel: Option<&str>, status: LiveRouteStatus, error: Option<&str>) -> LiveRoute {
        LiveRoute {
            id: channel.map_or("profiles".into(), |c| format!("channel:{c}")),
            channel: channel.map(Into::into),
            status,
            replay: Replay::Unknown,
            error: error.map(Into::into),
        }
    }

    fn view(status: LiveConnection, routes: Vec<LiveRoute>) -> LiveView {
        LiveView {
            live: LiveSnapshot {
                status,
                routes,
                error: None,
                retry_in: None,
            },
            roster: RosterRefresh::Verified,
            heads: Vec::new(),
        }
    }

    #[test]
    fn live_lines_follow_live_status() {
        let live = view(
            LiveConnection::Connected,
            vec![
                route(None, LiveRouteStatus::Live, None),
                route(Some("c"), LiveRouteStatus::Live, None),
            ],
        );
        assert_eq!(
            live_lines(&live, Some("c")),
            ("Live updates: stream established".into(), None)
        );
        let pending = view(
            LiveConnection::Connected,
            vec![route(Some("c"), LiveRouteStatus::Pending, None)],
        );
        assert_eq!(
            live_lines(&pending, Some("c")).0,
            "Live updates: connecting"
        );
        let quota = "rate-limited: quota exceeded; retry in 5s";
        let recovering = view(
            LiveConnection::Connected,
            vec![route(Some("c"), LiveRouteStatus::Pending, Some(quota))],
        );
        assert_eq!(
            live_lines(&recovering, Some("c")),
            (
                "Live updates: recovering automatically after rate limiting; awaiting \
                 confirmation"
                    .into(),
                Some(quota.into())
            )
        );
        let mut failed = view(LiveConnection::Error, Vec::new());
        failed.live.error = Some("Live reconnect attempts exhausted; retry available".into());
        failed.heads.push(HeadCatchUp {
            channel: "c".into(),
            state: CatchUpState::Verified,
            error: None,
        });
        let (line, rejection) = live_lines(&failed, Some("c"));
        assert_eq!(
            line,
            "Live updates: Live reconnect attempts exhausted; retry available"
        );
        assert_eq!(
            rejection.as_deref(),
            Some("Live reconnect attempts exhausted; retry available")
        );
    }

    #[test]
    fn a_quick_reconnect_stays_quiet_and_a_long_one_shows() {
        let mut quick = view(LiveConnection::Retrying, Vec::new());
        quick.live.error = Some("Live connection interrupted".into());
        quick.live.retry_in = Some(std::time::Duration::from_secs(1));
        assert_eq!(quick.issue(Some("c")), None);
        assert_eq!(live_lines(&quick, Some("c")).0, "Live updates: connecting");
        let mut long = quick.clone();
        long.live.retry_in = Some(std::time::Duration::from_secs(8));
        assert_eq!(
            long.issue(Some("c")).as_deref(),
            Some("Live connection interrupted")
        );
    }

    #[test]
    fn labels_match_buzz_app() {
        assert_eq!(
            capability_label(ReadCapability::FrontierSync),
            "frontier-sync"
        );
        assert_eq!(capability_label(ReadCapability::ReadOnly), "read-only");
        assert_eq!(sync_status_label(ReadSyncStatus::Reconciled), "reconciled");
    }
}
