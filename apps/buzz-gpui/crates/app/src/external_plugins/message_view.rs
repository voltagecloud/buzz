//! A message a plugin view shows as Buzz draws it in a conversation
//! (`conversation.ui.Message`): byline, text, attachments and reactions,
//! with the conversation's own action bar, reactions, copying, read
//! state, follows, delete and report. Replying, editing and opening its
//! thread go to the plugin, which decides where they happen (`onReply`,
//! `onOpenThread`).

use crate::community::Community;
use crate::runtime::Runtime;
use crate::ui::markdown::Directory;
use crate::ui::message::{self, Item, Rows};
use crate::ui::message_actions::{Hooks, MessageActions};
use buzz_relay_client::fold::MessageRow;
use buzz_relay_client::session::ThreadView;
use buzz_relay_client::{EventId, ThreadStatus};
use buzz_ui::{ActiveTheme, TypeStyled};
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// Tells the plugin's view what happened on the message.
pub type Send = Rc<dyn Fn(&str, serde_json::Value, &mut App)>;

/// How a `message` element draws its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub layout: message::Layout,
    /// No avatar or byline.
    pub compact: bool,
    /// The hover bar and its menu.
    pub actions: bool,
}

pub struct MessageView {
    community: Entity<Community>,
    style: Style,
    channel: String,
    row: Option<Arc<MessageRow>>,
    /// The message could not be read.
    missing: bool,
    actions: Entity<MessageActions>,
    _thread: ThreadView,
    _follow: Task<()>,
    _actions: Subscription,
}

fn ids(row: &MessageRow) -> serde_json::Value {
    serde_json::json!({
        "message": row.id.to_hex(),
        "root": row.thread_root_id.map(|root| root.to_hex()),
        "channel": &*row.channel,
    })
}

impl MessageView {
    /// Shows `message` in `channel`, following edits and reactions.
    pub fn new(
        community: Entity<Community>,
        channel: String,
        message: EventId,
        send: Send,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread = community
            .read(cx)
            .session
            .open_thread(&channel, message, true);
        let mut receiver = thread.snapshot.clone();
        let (sender, mut snapshots) = futures::channel::mpsc::unbounded();
        let _ = sender.unbounded_send(receiver.borrow_and_update().clone());
        Runtime::handle(cx).spawn(async move {
            while receiver.changed().await.is_ok() {
                let snapshot = receiver.borrow_and_update().clone();
                if sender.unbounded_send(snapshot).is_err() {
                    return;
                }
            }
        });
        let follow = cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(snapshot) = snapshots.next().await {
                let row = snapshot
                    .root
                    .iter()
                    .chain(snapshot.replies.iter())
                    .find(|row| row.id == message)
                    .cloned();
                let missing = row.is_none() && snapshot.status == ThreadStatus::Error;
                if this
                    .update(cx, |this, cx| {
                        this.row = row;
                        this.missing = missing;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let find = cx.entity().downgrade();
        let (reply, open, edit) = (send.clone(), send.clone(), send);
        let hooks = Hooks {
            reply: Rc::new(move |row, _, cx| reply("reply", ids(&row), cx)),
            open_thread: Some(Rc::new(move |row, _, cx| open("openThread", ids(&row), cx))),
            edit: Rc::new(move |row, _, cx| {
                let mut value = ids(&row);
                value["content"] = row.content.clone().into();
                edit("edit", value, cx)
            }),
            focus_composer: Rc::new(|_, _| {}),
            thread_root: None,
            find: Rc::new(move |id, cx| {
                find.upgrade()
                    .and_then(|view| view.read(cx).row.clone())
                    .filter(|row| row.id == id)
            }),
            thread: None,
            composer: WeakEntity::new_invalid(),
        };
        let actions = {
            let (community, channel) = (community.clone(), channel.clone());
            cx.new(|cx| MessageActions::new(community, channel, hooks, window, cx))
        };
        let observe = cx.observe(&actions, |_, _, cx| cx.notify());
        Self {
            community,
            channel,
            row: None,
            missing: false,
            style: Style {
                layout: message::Layout::Timeline,
                compact: false,
                actions: true,
            },
            actions,
            _thread: thread,
            _follow: follow,
            _actions: observe,
        }
    }
}

impl MessageView {
    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl Render for MessageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let Some(row) = self.row.clone() else {
            let text = if self.missing {
                "This message is not available."
            } else {
                "Loading message…"
            };
            return div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element();
        };
        let community = self.community.read(cx);
        let members = community
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
            .map(|channel| channel.members.clone());
        let mut rows = Rows::new(
            community.session.viewer(),
            Directory::of(community, members.as_deref(), cx),
        );
        if self.style.actions {
            MessageActions::fill(&self.actions, &mut rows, cx);
        }
        let rows = Rc::new(rows);
        let item = Item {
            row,
            head: !self.style.compact,
            day: None,
            group: None,
        };
        let overlays = if self.style.actions {
            self.actions
                .update(cx, |actions, cx| actions.overlays(window, cx))
        } else {
            Vec::new()
        };
        div()
            .relative()
            .child(message::render_in(self.style.layout, &item, &rows, cx))
            .children(overlays)
            .into_any_element()
    }
}
