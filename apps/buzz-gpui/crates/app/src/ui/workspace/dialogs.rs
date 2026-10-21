//! The workspace's dialogs (search, create channel, new message, add a
//! community) and opening a channel at a message.

use super::Workspace;
use crate::ui::add_community::AddCommunity;
use crate::ui::modal::{Dismissed, OpenChannel, OpenConversation};
use crate::ui::navigation::{ChannelRoute, Place, Target};
use crate::ui::new_channel::NewChannelDialog;
use crate::ui::new_message::{NewMessagePage, OnStarted};
use crate::ui::onboarding::Step;
use crate::ui::overlay::{self, Dismiss};
use crate::ui::pages::Page;
use crate::ui::search::{OpenPage, SearchDialog};
use crate::ui::sidebar;
use buzz_relay_client::{ChannelSummary, EventId};
use gpui_kit::*;
use std::rc::Rc;

/// The dialog open over the workspace.
pub(super) struct OpenDialog {
    view: AnyView,
    /// The search palette draws its own placement over the scrim.
    palette: bool,
    _subscription: Subscription,
}

impl Workspace {
    /// Opens `channel` and, with `message`, scrolls to it, loading older
    /// history as needed; a message not in the channel timeline (a thread
    /// reply) opens in the thread panel instead.
    pub fn open_channel_at(
        &mut self,
        channel: &ChannelSummary,
        message: Option<EventId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_channel(channel, window, cx);
        self.routed = None;
        self.place = (message, None);
        self.record(cx);
        if let Some(message) = message {
            self.reveal(message, None, window, cx);
        }
    }

    fn opener(cx: &mut Context<Self>) -> OpenChannel {
        let weak = cx.entity().downgrade();
        Rc::new(move |channel, message, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_channel_at(channel, message, window, cx)
            });
        })
    }

    fn show<V: Render + EventEmitter<Dismissed>>(
        &mut self,
        view: Entity<V>,
        palette: bool,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe(&view, |this, _, _: &Dismissed, cx| {
            this.dialog = None;
            cx.notify();
        });
        self.dialog = Some(OpenDialog {
            view: view.into(),
            palette,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Opens a plugin's dialog over the workspace, such as Remind me.
    pub(crate) fn show_dialog<V: Render + EventEmitter<Dismissed>>(
        &mut self,
        view: Entity<V>,
        cx: &mut Context<Self>,
    ) {
        self.show(view, false, cx);
    }

    /// Search Buzz. Without a ready community it offers pages only.
    /// Mod+K, unless a dialog is open.
    pub fn search_buzz(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_none() {
            self.open_search(window, cx);
        }
    }

    /// Mod+F, unless a dialog is open.
    pub fn search_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_none() {
            self.open_conversation_search(window, cx);
        }
    }

    pub(super) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search(false, window, cx);
    }

    /// Search this conversation (Mod+F), when a conversation is open.
    pub(super) fn open_conversation_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::MESSAGES && self.channel.is_some() {
            self.search(true, window, cx);
        }
    }

    fn search(&mut self, scoped: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Without an identity there is nothing to search.
        if matches!(self.onboarding, Some((Step::Identity, _))) {
            return;
        }
        let community = self.community.clone();
        let current = self
            .channel
            .as_ref()
            .filter(|_| self.page == Page::MESSAGES)
            .map(|page| page.read(cx).channel().clone());
        let scope = current.clone().filter(|_| scoped);
        let on_open = Self::opener(cx);
        let weak = cx.entity().downgrade();
        let on_page: OpenPage = Rc::new(move |page, window, cx| {
            let _ = weak.update(cx, |this, cx| match page {
                Page::MESSAGES => this.go_home(window, cx),
                Page::SETTINGS => this.open_settings(None, window, cx),
                page => this.select_page(page, window, cx),
            });
        });
        let dialog =
            cx.new(|cx| SearchDialog::new(community, current, scope, on_open, on_page, window, cx));
        self.show(dialog, true, cx);
    }

    pub(super) fn open_new_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = self.community.clone() else {
            return;
        };
        let on_open = Self::opener(cx);
        let dialog = cx.new(|cx| NewChannelDialog::new(community, on_open, window, cx));
        self.show(dialog, false, cx);
    }

    /// New message: a page of Messages (`routes.ts` `new-message`).
    pub(super) fn open_new_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = &self.community else {
            return;
        };
        let target = Target {
            community: Some(community.read(cx).origin.to_string()),
            place: Place::Channels(ChannelRoute::NewMessage),
        };
        self.navigate(target, window, cx);
    }

    /// Shows the New message page, made for the shown community.
    pub(super) fn show_new_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = self.community.clone() else {
            return;
        };
        if self.new_message.is_some() {
            return;
        }
        let weak = cx.entity().downgrade();
        let on_open: OpenConversation = Rc::new(move |id, window, cx| {
            let _ = weak.update(cx, |this, cx| this.open_conversation(id, window, cx));
        });
        let weak = cx.entity().downgrade();
        let on_started: OnStarted = Rc::new(move |channel, message, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.new_message = None;
                let origin = this
                    .community
                    .as_ref()
                    .map(|community| community.read(cx).origin.to_string());
                let target = Target {
                    community: origin,
                    place: Place::Conversation {
                        channel,
                        message: Some(message),
                        thread: None,
                        members: false,
                    },
                };
                this.navigate(target, window, cx);
            });
        });
        self.new_message =
            Some(cx.new(|cx| NewMessagePage::new(community, on_open, on_started, window, cx)));
    }

    pub(crate) fn open_add_community(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| AddCommunity::new(false, window, cx));
        self.show(dialog, false, cx);
    }

    /// An invite link: the add-community dialog, already checking it.
    pub(super) fn open_invite(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| AddCommunity::new(false, window, cx));
        dialog.update(cx, |dialog, cx| dialog.submit(link, window, cx));
        self.show(dialog, false, cx);
    }

    /// The page closed itself (the viewer left, hid or deleted the
    /// channel): the first other sidebar row opens instead, as in
    /// buzz-app's sidebar, or nothing when there is none.
    pub(super) fn close_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closed = self
            .channel
            .take()
            .map(|page| page.read(cx).channel_id().to_string());
        self.leave_channel_tabs();
        let Some(community) = &self.community else {
            cx.notify();
            return;
        };
        let snapshot = community.read(cx).snapshot.clone();
        match sidebar::first_row(&snapshot, closed.as_deref().unwrap_or_default()) {
            Some(next) => self.select_channel(&next, window, cx),
            None => community.read(cx).session.select(None),
        }
        cx.notify();
    }

    pub(super) fn dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.dialog.as_ref()?;
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.dialog = None;
                cx.notify();
            });
        });
        let content = if open.palette {
            // The palette fills the scrim and places itself near the top.
            div()
                .w(window.viewport_size().width)
                .h(window.viewport_size().height)
                .child(open.view.clone())
                .into_any_element()
        } else {
            open.view.clone().into_any_element()
        };
        Some(overlay::modal(window, content, Some(dismiss)).into_any_element())
    }
}
