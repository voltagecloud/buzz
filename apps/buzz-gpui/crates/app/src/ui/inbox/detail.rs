//! The Inbox detail (`InboxDetail.tsx`, `ChannelPreview.tsx`): the
//! selected conversation with its composer, revealed at the row's
//! message, or its thread for replies.

use crate::community::Community;
use crate::ui::channel::{ChannelPage, OpenThread, Reveal};
use crate::ui::navigation::{Place, Request, Target, with_workspace};
use crate::ui::thread::{OnClose, ThreadPanel};
use buzz_relay_client::read_state::ReadTarget;
use buzz_relay_client::unread::InboxItem;
use buzz_relay_client::{ChannelSummary, ChannelType, EventId};
use buzz_ui::{
    ActiveTheme, IconButton, IconButtonSize, IconName, PanelHeader, TypeStyled, tokens::space,
};
use gpui_kit::*;
use std::sync::Arc;

/// The row's messages still wait for their edits and deletions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Incomplete {
    Loading,
    Error,
}

/// The detail asks the page to close it.
pub struct Closed;

/// The header's Archive or Restore conversation.
pub struct ToggleArchive;

/// What the header's archive button shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArchiveAction {
    pub archived: bool,
    /// An Inbox action is running.
    pub disabled: bool,
}

enum Reader {
    Channel(Entity<ChannelPage>),
    Thread(Entity<ThreadPanel>),
}

impl Reader {
    fn element(&self) -> AnyElement {
        match self {
            Reader::Channel(page) => page.clone().into_any_element(),
            Reader::Thread(panel) => panel.clone().into_any_element(),
        }
    }
}

pub struct InboxDetail {
    community: Entity<Community>,
    item: InboxItem,
    incomplete: Option<Incomplete>,
    archive: ArchiveAction,
    /// Made once the conversation is available and settled; kept while a
    /// later check withholds it, so its scroll and draft survive.
    reader: Option<Reader>,
    /// The message to reveal once the channel's rows load.
    reveal: Option<EventId>,
    /// The message and thread root the row had when selected, which the
    /// reader anchors on and opens (`selectedTarget`): reading moves the
    /// row's own message on.
    anchor: (String, Option<String>),
    /// The message the channel's newest rows did not hold, shown in a
    /// thread reader instead, once (`ChannelPreview`'s fallback).
    fallback: Option<EventId>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Closed> for InboxDetail {}
impl EventEmitter<ToggleArchive> for InboxDetail {}

impl InboxDetail {
    pub fn new(
        community: Entity<Community>,
        item: InboxItem,
        incomplete: Option<Incomplete>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut detail = Self {
            _subscriptions: vec![cx.observe(&community, |_, _, cx| cx.notify())],
            community,
            anchor: (item.message_id.clone(), item.root_id.clone()),
            item,
            incomplete,
            archive: ArchiveAction::default(),
            reader: None,
            reveal: None,
            fallback: None,
            focus,
        };
        detail.admit(window, cx);
        detail
    }

    /// The page's current row for this visit and its preview state.
    pub fn update_item(
        &mut self,
        item: InboxItem,
        incomplete: Option<Incomplete>,
        archive: ArchiveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.archive != archive {
            self.archive = archive;
            cx.notify();
        }
        if self.item == item && self.incomplete == incomplete {
            return;
        }
        self.item = item;
        self.incomplete = incomplete;
        // The reader stays while the preview updates, but is not shown.
        let presented = incomplete.is_none();
        match &self.reader {
            Some(Reader::Channel(page)) => ChannelPage::set_presented(page, presented, cx),
            Some(Reader::Thread(panel)) => ThreadPanel::set_presented(panel, presented, cx),
            None => {}
        }
        self.admit(window, cx);
        cx.notify();
    }

    fn channel(&self, cx: &App) -> Option<ChannelSummary> {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.item.channel_id)
            .cloned()
    }

    fn available(&self, cx: &App) -> bool {
        let viewer = self.community.read(cx).session.viewer().to_string();
        self.channel(cx).is_some_and(|channel| {
            !channel.cached
                && !channel.archived
                && !channel.read_only
                && channel.members.contains(&viewer)
        })
    }

    /// Makes the reader once the conversation is available and settled.
    fn admit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reader.is_some() || self.incomplete.is_some() || !self.available(cx) {
            return;
        }
        let Some(channel) = self.channel(cx) else {
            return;
        };
        let community = self.community.clone();
        let message = EventId::from_hex(&self.anchor.0).ok();
        match (&self.item.target, message) {
            (ReadTarget::Channel { .. }, message) => {
                let origin = community.read(cx).origin.to_string();
                // A thread opened from the preview opens in Channels.
                let on_open_thread: OpenThread = Arc::new(move |row, _, _, cx| {
                    let target = Target {
                        community: Some(origin.clone()),
                        place: Place::Conversation {
                            channel: row.channel.to_string(),
                            message: Some(row.thread_root_id.unwrap_or(row.id)),
                            thread: Some(row.thread_root_id.unwrap_or(row.id)),
                            members: false,
                        },
                    };
                    with_workspace(cx, move |workspace, window, cx| {
                        workspace.handle(Request::Open(target), window, cx)
                    });
                });
                let page = cx.new(|cx| {
                    let mut page =
                        ChannelPage::new(community.clone(), channel, on_open_thread, window, cx);
                    page.secondary = true;
                    // A reader's links open nowhere in Buzz
                    // (`onOpenLink={() => false}`), nor do its threads.
                    page.as_reader(cx);
                    page
                });
                self._subscriptions
                    .push(cx.observe(&page, |this, _, cx| this.try_reveal(cx)));
                self.reveal = message;
                self.reader = Some(Reader::Channel(page));
                self.try_reveal(cx);
            }
            (_, Some(message)) => {
                let panel = self.thread_reader(&channel.id, message, window, cx);
                self.reader = Some(Reader::Thread(panel));
            }
            (_, None) => {}
        }
    }

    /// A thread reader revealing `message`.
    fn thread_reader(
        &self,
        channel: &str,
        message: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadPanel> {
        let page = cx.entity().downgrade();
        let on_close: OnClose = Arc::new(move |_, cx| {
            let _ = page.update(cx, |_, cx| cx.emit(Closed));
        });
        let community = self.community.clone();
        let channel = channel.to_owned();
        cx.new(|cx| {
            let mut panel = ThreadPanel::exact(community, channel, message, on_close, window, cx);
            panel.as_reader(cx);
            panel
        })
    }

    /// A message the channel reader could not show opens in a thread
    /// reader instead, decided once.
    fn take_fallback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(message) = self.fallback.take() else {
            return;
        };
        if !matches!(self.reader, Some(Reader::Channel(_))) {
            return;
        }
        let channel = self.item.channel_id.clone();
        let panel = self.thread_reader(&channel, message, window, cx);
        // It is no session conversation's own view (`ChannelPreview`
        // passes no `sessionConversation`).
        panel
            .read(cx)
            .composer_entity()
            .update(cx, |composer, _| composer.not_conversation_view());
        self.reader = Some(Reader::Thread(panel));
    }

    fn try_reveal(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(Reader::Channel(page))) = (self.reveal, &self.reader) else {
            return;
        };
        let page = page.clone();
        let outcome = page.update(cx, |page, cx| page.reveal(id, cx));
        if outcome != Reveal::Loading {
            self.reveal = None;
        }
        // Not among the newest rows (older, a reply, or a saved window).
        if outcome == Reveal::Absent {
            self.fallback = Some(id);
            cx.notify();
        }
    }

    fn open(&self, cx: &mut Context<Self>) {
        let root = self
            .anchor
            .1
            .as_deref()
            .and_then(|root| EventId::from_hex(root).ok());
        let target = Target {
            community: Some(self.community.read(cx).origin.to_string()),
            place: Place::Conversation {
                channel: self.item.channel_id.clone(),
                message: EventId::from_hex(&self.anchor.0).ok(),
                thread: root,
                members: false,
            },
        };
        with_workspace(cx, move |workspace, window, cx| {
            workspace.handle(Request::Open(target), window, cx)
        });
    }
}

impl Render for InboxDetail {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        // A conversation no longer available takes its reader with it, and
        // a new one is made if it comes back (`admitted` resets).
        if self.reader.is_some() && !self.available(cx) {
            self.reader = None;
            self.reveal = None;
            self.fallback = None;
        }
        self.admit(window, cx);
        self.take_fallback(window, cx);
        let channel = self.channel(cx);
        let dm = channel
            .as_ref()
            .is_some_and(|channel| channel.channel_type == Some(ChannelType::Dm));
        let title = if dm {
            "Direct message".to_string()
        } else {
            channel
                .as_ref()
                .map(|channel| channel.name.clone())
                .unwrap_or_else(|| "Conversation".into())
        };
        let available = self.available(cx);
        let notice = match self.incomplete {
            Some(Incomplete::Error) => Some("Preview unavailable. Retry inbox."),
            Some(Incomplete::Loading) => Some("Preview updating…"),
            None if !available => {
                Some("This conversation is unavailable. Open it in Channels to check access.")
            }
            None => None,
        };
        let reader = (available && self.incomplete.is_none())
            .then(|| self.reader.as_ref().map(Reader::element))
            .flatten();
        div()
            .id("inbox-detail")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(Closed);
                }
            }))
            .child(
                PanelHeader::new()
                    .joined(true)
                    .label(
                        Some(if dm {
                            IconName::ChatCircle
                        } else {
                            IconName::Hash
                        }),
                        title,
                    )
                    .label_link(
                        "inbox-detail-heading",
                        if dm {
                            "Open conversation"
                        } else if self.item.root_id.is_some() {
                            "Open full thread"
                        } else {
                            "Open in channel"
                        },
                        cx.listener(|this, _, _, cx| this.open(cx)),
                    )
                    .action(
                        IconButton::new(
                            "inbox-archive",
                            if self.archive.archived {
                                IconName::ArchiveOff
                            } else {
                                IconName::Archive
                            },
                        )
                        .size(IconButtonSize::Sm)
                        .disabled(self.archive.disabled || !available || self.incomplete.is_some())
                        .tooltip(if self.archive.archived {
                            "Restore conversation"
                        } else {
                            "Archive conversation"
                        })
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ToggleArchive))),
                    )
                    .action(
                        IconButton::new("inbox-open", IconName::ArrowSquareOut)
                            .size(IconButtonSize::Sm)
                            .tooltip("Open in channel")
                            .on_click(cx.listener(|this, _, _, cx| this.open(cx))),
                    )
                    .action(
                        IconButton::new("inbox-close", IconName::X)
                            .size(IconButtonSize::Sm)
                            .tooltip("Close detail")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Closed))),
                    ),
            )
            .children(notice.map(|text| {
                div()
                    .py(space::S3)
                    .px(space::PANEL_INSET)
                    .text_body()
                    .text_color(c.text_standard)
                    .child(text)
            }))
            .child(div().flex().flex_col().flex_1().min_h_0().children(reader))
    }
}
