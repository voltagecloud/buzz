//! The Channels page's routes without a conversation (`routes.ts`) and
//! what happens when the sidebar's row menu removes the open channel.

use super::Workspace;
use crate::community::AppState;
use crate::ui::navigation::ChannelRoute;
use crate::ui::pages::Page;
use crate::ui::sidebar;
use gpui_kit::*;

/// View state key of the channel last open in a community.
const SELECTED_CHANNEL: &str = "selected-channel";

impl Workspace {
    /// What Messages says without a conversation.
    pub(super) fn route_placeholder(&self) -> &'static str {
        match self.route {
            Some(ChannelRoute::NewSession { .. }) => {
                "Start new conversations from Me. Channel-nested sessions are no longer available."
            }
            _ => "Select a channel to read it.",
        }
    }

    pub(super) fn show_route(
        &mut self,
        route: ChannelRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_page(Page::MESSAGES, window, cx);
        self.retire_menu_tabs(cx);
        self.channel = None;
        self.leave_channel_tabs();
        if route == ChannelRoute::NewMessage {
            self.show_new_message(window, cx);
        } else {
            self.new_message = None;
        }
        if let Some(community) = &self.community {
            let community = community.read(cx);
            community.session.select(None);
            // The saved selection must not reopen what was just left.
            if route == ChannelRoute::Empty {
                let _ = cx.global::<AppState>().store.set_view_state(
                    &community.partition(),
                    SELECTED_CHANNEL,
                    None,
                );
            }
        }
        self.route = Some(route);
        cx.notify();
    }

    /// The viewer left, hid or deleted `channel` from its sidebar row: if
    /// it is open, the next sidebar row opens, or the empty route.
    pub(super) fn channel_removed(
        &mut self,
        channel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self
            .channel
            .as_ref()
            .is_some_and(|page| page.read(cx).channel_id() == channel);
        let Some(community) = self.community.clone().filter(|_| open) else {
            return;
        };
        let snapshot = community.read(cx).snapshot.clone();
        match sidebar::first_row(&snapshot, channel) {
            Some(next) => self.select_channel(&next, window, cx),
            None => {
                self.show_route(ChannelRoute::Empty, window, cx);
                self.record(cx);
            }
        }
    }
}
