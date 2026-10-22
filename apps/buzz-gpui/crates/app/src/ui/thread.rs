//! The thread panel in the Messages board's right dock (ui.md §4.11).

use super::channel_menu::{JoinNotice, Joining};
use super::composer::{Composer, ComposerEvent, ReplyContext, Submission};
use super::jump::{self, Jump};
use super::markdown::Directory;
use super::message::{self, Item, Layout, Rows};
use super::message_actions::{Hooks, MessageActions};
use super::reading::{self, Drawn, Reporter};
use super::typing::{self, Activity, AgentChooser};
use super::{media, time};
use crate::community::Community;
use crate::mirror::mirror;
use buzz_relay_client::fold::{MessageRow, ReplyTree};
use buzz_relay_client::session::ThreadView;
use buzz_relay_client::threads::{Direction, ReadKind};
use buzz_relay_client::{
    ChannelSummary, ChannelType, Media, Profiles, Retarget, TargetStatus, ThreadSnapshot,
    ThreadStatus, UnreadDetail, UnreadWatch,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, IconName, PanelHeader, Tab, TabVariant,
    Tabs, TypeStyled,
    tokens::{duration, easing},
};
use chrono::NaiveDate;
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::prelude::EventId;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Older replies load when the reader is this many rows from the top.
const LOAD_OLDER_WITHIN: usize = 3;
/// Child lists indent by one control width.
const INDENT: f32 = 40.;
/// Indentation stops growing at this depth; ancestry still nests.
const MAX_INDENT: usize = 6;
/// The connector spine runs under the centre of a 32px avatar.
const SPINE: f32 = 16.;

/// How deep indentation may grow in a panel this wide: it flattens at
/// depth 3 up to 360px and at depth 4 up to 480px, and not at all up to
/// 320px (`Messages.module.css`'s `messages` container queries).
fn max_indent(width: Pixels) -> usize {
    match f32::from(width) {
        w if w <= 0. => MAX_INDENT,
        w if w <= 320. => 0,
        w if w <= 360. => 3,
        w if w <= 480. => 4,
        _ => MAX_INDENT,
    }
}

/// Asks the workspace to close the panel.
pub type OnClose = Arc<dyn Fn(&mut Window, &mut App)>;
/// Tells the host the message it asked this panel to select belongs to
/// another thread (`onDifferentThread`): the requested message, and the
/// one the panel still shows.
pub type OnDifferent = Rc<dyn Fn(EventId, EventId, &mut Window, &mut App)>;

/// The thread composer's placeholder. It never addresses the viewer:
/// a thread they started, as every agent exchange is, stays neutral.
fn reply_placeholder(root: &MessageRow, viewer: &str, name: &str) -> SharedString {
    let starter = root.workflow_owner.unwrap_or(root.author);
    if starter.to_hex() == viewer {
        "Reply in thread".into()
    } else {
        format!("Reply in thread to {name}").into()
    }
}

/// How the viewer stands in a thread's conversation.
enum Access {
    Member,
    /// A public preview the viewer may join.
    Joinable(ChannelSummary),
    /// A preview they cannot join here.
    Preview(ChannelSummary),
    Other,
}

/// A same-thread link that could not open; the thread stays usable.
struct SelectionFailure {
    message: EventId,
    /// Gone for good, so Try again would not help.
    unavailable: bool,
    /// Try again is running.
    pending: bool,
}

/// A reply just sent, kept in sight once it lands.
struct Sent {
    parent: Option<EventId>,
    /// Replies there before it.
    known: HashSet<EventId>,
}

/// Replies seen once the thread was complete, to count arrivals for the
/// jump pill (`previousReplies`).
#[derive(Default)]
struct Seen {
    ids: HashSet<EventId>,
    latest: u64,
    complete: bool,
}

pub struct ThreadPanel {
    community: Entity<Community>,
    viewer: String,
    channel: String,
    root_id: EventId,
    /// How links in its rows open: a reader's open nowhere in Buzz.
    link_mode: crate::ui::links::LinkMode,
    view: ThreadView,
    snapshot: Arc<ThreadSnapshot>,
    entries: Vec<Entry>,
    list: ListState,
    /// The row holding keyboard focus last frame.
    keyboard_row: Option<EventId>,
    /// Nested branches the reader opened; others stay collapsed.
    expanded: HashSet<EventId>,
    /// The reply the composer answers, when not the root.
    reply_parent: Option<EventId>,
    composer: Entity<Composer>,
    input: Entity<TextareaState>,
    /// Row actions: the hover bar, menus, reactions and dialogs.
    message_actions: Entity<MessageActions>,
    error: Option<SharedString>,
    on_close: OnClose,
    /// Draws its own "Thread" header. Off inside a tab pane, whose tab
    /// strip names and closes it.
    pub show_header: bool,
    /// A saved draft opened this thread (`requireReadyRoot`): the composer
    /// waits until the loaded root is the draft's own.
    pub require_ready_root: bool,
    /// Exact mode: the message to reveal once its thread has loaded.
    revealing: Option<EventId>,
    /// A link's message being selected in this thread, then the one
    /// selected (`RetainedThreadPanel`).
    requested: Option<EventId>,
    selected: Option<EventId>,
    selection_failure: Option<SelectionFailure>,
    /// The message last asked for: opened on, or passed to
    /// [`select`](Self::select), even if that failed.
    asked: EventId,
    pub on_different: Option<OnDifferent>,
    /// The conversation as its host knows it, for one not in the joined
    /// list, such as a public preview.
    pub summary: Option<ChannelSummary>,
    /// A preview's Join prompt, in place of the composer.
    join: Option<Entity<JoinNotice>>,
    /// When the panel opened, for its loading mark.
    opened: std::time::Instant,
    /// When its first page landed and its rows first showed; until then
    /// the loading mark holds the thread's place.
    presented: Option<std::time::Instant>,
    /// The reader started joining this preview.
    joining: bool,
    _selecting: Option<Task<()>>,
    /// The panel, focused when it opens; focus here (the history or the
    /// composer) is reading.
    focus: FocusHandle,
    reporter: Reporter,
    /// Where the replies were drawn, for the reporter.
    drawn: Drawn,
    jump: Jump,
    seen: Seen,
    /// Unread replies, for the "(N new)" of collapsed branches.
    unread_watch: UnreadWatch,
    unread: Arc<UnreadDetail>,
    watched: Vec<EventId>,
    /// The viewer's agents working in this thread.
    activity: Entity<AgentChooser>,
    /// The reply at the top while an older page loads, with the ancestors
    /// it had then: ancestors the page brings are opened around it.
    older_anchor: Option<(EventId, Vec<EventId>)>,
    sent: Option<Sent>,
    /// A newly opened branch's first reply, scrolled into view once laid
    /// out.
    reveal_child: Option<EventId>,
    /// The panel's width, which bounds indentation.
    width: Pixels,
    day: NaiveDate,
    _watch: Task<()>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadPanel {
    /// How far the exact target it opened at got (§5c.5); `None` for a
    /// thread opened at its root.
    pub fn target_status(&self) -> Option<TargetStatus> {
        self.snapshot.target_status
    }

    /// Whether its root has loaded: `None` while it loads, `Some(false)`
    /// when the read finished without it or failed.
    pub fn root_found(&self) -> Option<bool> {
        use buzz_relay_client::threads::ThreadStatus;
        match self.snapshot.status {
            ThreadStatus::Idle | ThreadStatus::Loading if self.snapshot.root.is_none() => None,
            _ => Some(self.snapshot.root.is_some()),
        }
    }

    /// A reader's thread: links open nowhere in Buzz, nor do its working
    /// agents' threads or activity (`onOpenLink={() => false}`).
    pub fn as_reader(&mut self, cx: &mut Context<Self>) {
        self.link_mode = crate::ui::links::LinkMode::Closed;
        self.activity.update(cx, |chooser, _| chooser.closed = true);
    }

    pub fn new(
        community: Entity<Community>,
        channel: String,
        root_id: EventId,
        on_close: OnClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::open(community, channel, root_id, false, on_close, window, cx)
    }

    /// Reveals `message`, any message, in its thread (relay.md §5c.5).
    pub fn exact(
        community: Entity<Community>,
        channel: String,
        message: EventId,
        on_close: OnClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::open(community, channel, message, true, on_close, window, cx)
    }

    fn open(
        community: Entity<Community>,
        channel: String,
        root_id: EventId,
        exact: bool,
        on_close: OnClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (view, unread_watch, viewer) = {
            let session = &community.read(cx).session;
            (
                session.open_thread(&channel, root_id, exact),
                session.watch_unread(&channel),
                session.viewer().to_string(),
            )
        };
        let snapshot = view.snapshot.borrow().clone();
        let watch = mirror(view.snapshot.clone(), cx, |this: &mut Self, value| {
            this.snapshot = value;
            this.snapshot_changed();
        });
        let list = ListState::new(0, ListAlignment::Top, px(1000.));
        list.set_follow_mode(FollowMode::Tail);
        let composer = cx.new(|cx| {
            Composer::new(
                community.clone(),
                channel.clone(),
                Some(root_id),
                SharedString::default(),
                window,
                cx,
            )
        });
        composer.update(cx, |composer, _| composer.conversation_view());
        let input = composer.read(cx).input();
        let focus = cx.focus_handle();
        let message_actions =
            Self::message_actions(&community, &channel, root_id, &composer, window, cx);
        let mut subscriptions = vec![
            cx.observe(&community, |_, _, cx| cx.notify()),
            cx.observe(&message_actions, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &ComposerEvent, window, cx| match event {
                    ComposerEvent::Submit(submission) => this.send(submission.clone(), window, cx),
                    ComposerEvent::CancelReply => this.reply_to(None, window, cx),
                    ComposerEvent::EditLast => this.edit_last(window, cx),
                    ComposerEvent::RetryEdit => this
                        .message_actions
                        .update(cx, |actions, cx| actions.retry_edit(cx)),
                },
            ),
            // Reading stops when the window is in the background.
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
            cx.on_focus_in(&focus, window, |_, _, cx| cx.notify()),
            cx.on_focus_out(&focus, window, |_, _, _, cx| cx.notify()),
        ];
        // A selected message is revealed once its thread has loaded.
        subscriptions.push(cx.observe_self(|this, cx| this.reveal_target(cx)));
        subscriptions.push(super::thread_banners::observe(cx));
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            if event.visible_range.start < LOAD_OLDER_WITHIN && event.is_scrolled {
                let _ = weak.update(cx, |this, _| this.load_older());
            }
        });
        let tasks = vec![
            time::on_new_day(cx, |_, cx| cx.notify()),
            mirror(
                unread_watch.snapshot.clone(),
                cx,
                |this: &mut Self, value| this.unread = value,
            ),
        ];
        let unread = unread_watch.snapshot.borrow().clone();
        let mut panel = Self {
            community,
            viewer,
            channel,
            root_id,
            link_mode: crate::ui::links::LinkMode::Normal,

            view,
            snapshot,
            entries: Vec::new(),
            list,
            keyboard_row: None,
            expanded: HashSet::new(),
            reply_parent: None,
            composer,
            input,
            message_actions,
            error: None,
            on_close,
            show_header: false,
            require_ready_root: false,
            revealing: exact.then_some(root_id),
            requested: None,
            selected: None,
            selection_failure: None,
            asked: root_id,
            on_different: None,
            summary: None,
            join: None,
            opened: std::time::Instant::now(),
            presented: None,
            joining: false,
            _selecting: None,
            focus,
            reporter: Reporter::default(),
            drawn: Drawn::default(),
            jump: Jump::default(),
            seen: Seen::default(),
            unread_watch,
            unread,
            watched: Vec::new(),
            activity: cx.new(AgentChooser::new),
            older_anchor: None,
            sent: None,
            reveal_child: None,
            width: px(0.),
            day: time::today(),
            _watch: watch,
            _tasks: tasks,
            _subscriptions: subscriptions,
        };
        panel.snapshot_changed();
        panel
    }

    /// Whether this reader is shown (`ConversationPresentation`).
    pub fn set_presented(this: &Entity<Self>, presented: bool, cx: &mut App) {
        let actions = this.read(cx).message_actions.clone();
        actions.update(cx, |actions, cx| actions.set_presented(presented, cx));
    }

    /// How the viewer stands in the thread's conversation: a member may
    /// reply; a public preview offers Join instead of the composer
    /// (`useListedChannel`).
    fn access(&self, cx: &App) -> Access {
        let community = self.community.read(cx);
        let listed = community
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel);
        let Some(channel) = listed.or(self.summary.as_ref()) else {
            return Access::Other;
        };
        if channel.read_only && !channel.cached {
            let joinable = !channel.archived
                && matches!(
                    channel.channel_type,
                    Some(ChannelType::Stream | ChannelType::Forum)
                );
            return match joinable {
                true => Access::Joinable(channel.clone()),
                false => Access::Preview(channel.clone()),
            };
        }
        match listed.is_some_and(|channel| !channel.read_only && !channel.archived) {
            true => Access::Member,
            false => Access::Other,
        }
    }

    /// The Buzz loading mark in the thread's place (`ThreadLoading`), sized
    /// to the area waiting, between 80 and 160px. It appears after a
    /// moment, so a thread that loads at once never flashes it.
    fn loading(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let height = self.list.viewport_bounds().size.height;
        let area = match (self.width > px(0.), height > px(0.)) {
            (true, true) => self.width.min(height),
            (true, false) => self.width,
            _ => px(480.),
        };
        let width = (area * 0.24).clamp(px(80.), px(160.));
        let background = cx.colors().surface_panel;
        let mark = super::launch::mark(self.opened, width, background, window, cx);
        let (delay, fade) = (duration::STATE.as_secs_f32(), duration::STATE.as_secs_f32());
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(mark.with_animation(
                ElementId::Name(format!("thread-loading-{}", cx.entity_id()).into()),
                Animation::new(std::time::Duration::from_secs_f32(delay + fade)),
                move |mark, t| {
                    let at = t * (delay + fade);
                    let shown = ((at - delay) / fade).clamp(0., 1.);
                    mark.opacity(easing::cubic_bezier(easing::ENTER, shown))
                },
            ))
            .into_any_element()
    }

    /// The message last asked of the panel, which a host compares with
    /// the message its tab wants shown.
    pub fn asked(&self) -> EventId {
        self.asked
    }

    /// Selects `message` without rebuilding the panel, so the reply being
    /// written, its target, the scroll position and the open branches all
    /// stay (`RetainedThreadPanel`). The session first verifies that the
    /// message belongs to this thread; one of another thread goes to
    /// [`on_different`](Self::on_different), and one that cannot open
    /// leaves a notice with the thread as it was. False until the root
    /// has loaded: nothing is known to share the thread before, and the
    /// host opens a new panel instead.
    pub fn select(
        &mut self,
        message: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.loaded_root().is_none() {
            return false;
        }
        self.asked = message;
        self.start_selection(message, false, window, cx);
        true
    }

    /// With `retry`, the failure notice stays, its Try again busy, until
    /// the new attempt ends.
    fn start_selection(
        &mut self,
        message: EventId,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &mut self.selection_failure {
            Some(failure) if retry => failure.pending = true,
            _ => self.selection_failure = None,
        }
        self.requested = Some(message);
        let outcome = self.view.retarget(message);
        self._selecting = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = outcome.await.unwrap_or(Retarget::Cancelled);
            let _ = this.update_in(cx, |this, window, cx| {
                this.selection_ended(message, outcome, window, cx)
            });
        }));
        cx.notify();
    }

    fn selection_ended(
        &mut self,
        message: EventId,
        outcome: Retarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.requested = None;
        match outcome {
            Retarget::Selected => {
                self.selection_failure = None;
                self.selected = Some(message);
                // The session published the new fold before answering.
                self.snapshot = self.view.snapshot.borrow().clone();
                self.snapshot_changed();
                // The thread's own root is no exact target: the reader
                // stays where they are, and so does the focus.
                if self.loaded_root() != Some(message) {
                    self.revealing = Some(message);
                    self.reveal_target(cx);
                    window.focus(&self.focus, cx);
                }
            }
            Retarget::Different => self.selection_failure = None,
            outcome => {
                self.selection_failure = Some(SelectionFailure {
                    message,
                    unavailable: outcome == Retarget::Unavailable,
                    pending: false,
                });
            }
        }
        if outcome == Retarget::Different
            && let Some(on_different) = self.on_different.clone()
        {
            let kept = self.selected.unwrap_or(self.root_id);
            self.asked = kept;
            on_different(message, kept, window, cx);
        }
        cx.notify();
    }

    fn retry_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(message) = self.selection_failure.as_ref().map(|f| f.message) {
            self.start_selection(message, true, window, cx);
        }
    }

    /// The thread's root once it has loaded; in exact mode [`Self::message_id`]
    /// may be a reply.
    pub fn loaded_root(&self) -> Option<EventId> {
        self.snapshot.root.as_ref().map(|root| root.id)
    }

    /// The composer itself, for hosts that follow its sends (Inbox drafts).
    pub fn composer_entity(&self) -> Entity<super::composer::Composer> {
        self.composer.clone()
    }

    /// The row actions, wired to this panel's composer.
    fn message_actions(
        community: &Entity<Community>,
        channel: &str,
        root_id: EventId,
        composer: &Entity<Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<MessageActions> {
        let panel = cx.entity().downgrade();
        let (reply, root, find) = (panel.clone(), panel.clone(), panel);
        let (edit_composer, focus_composer) = (composer.downgrade(), composer.downgrade());
        let hooks = Hooks {
            reply: Rc::new(move |row, window, cx| {
                let _ = reply.update(cx, |this, cx| {
                    let parent = (Some(row.id) != this.snapshot.root.as_ref().map(|r| r.id))
                        .then_some(row.id);
                    this.reply_to(parent, window, cx)
                });
            }),
            open_thread: None,
            edit: Rc::new(move |row, window, cx| {
                let _ =
                    edit_composer.update(cx, |composer, cx| composer.start_edit(row, window, cx));
            }),
            focus_composer: Rc::new(move |window, cx| {
                let _ = focus_composer.update(cx, |composer, cx| composer.focus(window, cx));
            }),
            thread_root: Some(Rc::new(move |cx| {
                root.upgrade()
                    .and_then(|panel| panel.read(cx).snapshot.root.clone())
            })),
            find: Rc::new(move |id, cx| {
                find.upgrade().and_then(|panel| {
                    let snapshot = &panel.read(cx).snapshot;
                    snapshot
                        .root
                        .iter()
                        .chain(snapshot.replies.iter())
                        .find(|row| row.id == id)
                        .cloned()
                })
            }),
            thread: Some(root_id),
            composer: composer.downgrade(),
        };
        let (community, channel) = (community.clone(), channel.to_string());
        cx.new(|cx| MessageActions::new(community, channel, hooks, window, cx))
    }

    /// Up in an empty composer edits the viewer's newest editable reply.
    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows: Vec<Arc<MessageRow>> = self
            .snapshot
            .root
            .iter()
            .chain(self.snapshot.replies.iter())
            .cloned()
            .collect();
        let row = self.message_actions.read(cx).last_editable(rows.iter(), cx);
        if let Some(row) = row {
            self.composer
                .update(cx, |composer, cx| composer.start_edit(row, window, cx));
        }
    }

    /// Focus moves to the panel as it opens, as to buzz-app's close
    /// button, so Escape closes it and the keys scroll it.
    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let focus = this.read(cx).focus.clone();
        window.focus(&focus, cx);
    }

    /// The Reply action opened the thread: focus goes to its composer.
    pub fn focus_composer(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The snapshot changed: count arrivals, open ancestors an older page
    /// brought, and rebuild the rows.
    fn snapshot_changed(&mut self) {
        let snapshot = self.snapshot.clone();
        let complete = snapshot.status == ThreadStatus::Ready && !snapshot.can_load_more;
        if complete || self.seen.complete {
            let arrivals = if complete && self.seen.complete {
                snapshot
                    .replies
                    .iter()
                    .filter(|reply| {
                        !self.seen.ids.contains(&reply.id) && reply.created_at >= self.seen.latest
                    })
                    .count()
            } else {
                0
            };
            if complete {
                self.seen = Seen {
                    ids: snapshot.replies.iter().map(|reply| reply.id).collect(),
                    latest: snapshot
                        .replies
                        .iter()
                        .map(|reply| reply.created_at)
                        .max()
                        .unwrap_or(0)
                        .max(self.seen.latest),
                    complete: true,
                };
            }
            if arrivals > 0 && !self.list.is_following_tail() {
                self.jump.arrived(arrivals);
            }
        }
        if let Some((anchor, before)) = &self.older_anchor
            && snapshot.status != ThreadStatus::Loading
        {
            let added = snapshot
                .tree
                .ancestors(anchor)
                .into_iter()
                .filter(|id| !before.contains(id));
            self.expanded.extend(added);
            self.older_anchor = None;
        }
        self.sync_entries();
        self.follow_sent();
    }

    /// Rebuilds the flattened rows, splicing only what changed so the
    /// reading position holds while older replies load above.
    fn sync_entries(&mut self) {
        let next = entries(&self.snapshot, &self.expanded);
        let prefix = self
            .entries
            .iter()
            .zip(&next)
            .take_while(|(a, b)| a.same(b))
            .count();
        let suffix = self.entries[prefix..]
            .iter()
            .rev()
            .zip(next[prefix..].iter().rev())
            .take_while(|(a, b)| a.same(b))
            .count();
        let old_end = self.entries.len() - suffix;
        self.list
            .splice(prefix..old_end, next.len() - suffix - prefix);
        self.entries = next;
        self.day = time::today();
    }

    /// Pages in older replies while the reader is near the top, keeping
    /// the reply at the top in place and opening ancestors that arrive.
    fn load_older(&mut self) {
        let snapshot = &self.snapshot;
        if snapshot.direction != Some(Direction::Older)
            || snapshot.status != ThreadStatus::Ready
            || !snapshot.can_load_more
        {
            return;
        }
        self.capture_older_anchor();
        self.view.load_more();
    }

    fn capture_older_anchor(&mut self) {
        self.list.pause_following_tail();
        let top = self.list.logical_scroll_top().item_ix;
        let anchor =
            self.entries[top.min(self.entries.len())..]
                .iter()
                .find_map(|entry| match entry {
                    Entry::Reply(reply) => Some(reply.item.row.id),
                    _ => None,
                });
        self.older_anchor = anchor.map(|anchor| (anchor, self.snapshot.tree.ancestors(&anchor)));
    }

    /// A sent reply has landed: its branch is open, so bring it into view.
    fn follow_sent(&mut self) {
        let Some(sent) = &self.sent else {
            return;
        };
        let viewer = &self.viewer;
        let landed = self.snapshot.replies.iter().find(|reply| {
            !sent.known.contains(&reply.id)
                && reply.author.to_hex() == *viewer
                && reply.reply_parent_id.filter(|p| Some(*p) != self.root()) == sent.parent
        });
        let Some(landed) = landed.map(|reply| reply.id) else {
            return;
        };
        self.sent = None;
        if let Some(index) = self.entries.iter().position(|e| e.row_id() == Some(landed)) {
            self.list.scroll_to_reveal_item(index);
        }
    }

    fn root(&self) -> Option<EventId> {
        self.snapshot.root.as_ref().map(|root| root.id)
    }

    /// Once the thread around the selected message has loaded, opens the
    /// branches above it, scrolls to it and moves focus to the panel
    /// (`useMessageReveal`: focus, no tint).
    fn reveal_target(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.revealing else {
            return;
        };
        let snapshot = &self.snapshot;
        if snapshot.target_status == Some(TargetStatus::Loading)
            || matches!(snapshot.status, ThreadStatus::Idle | ThreadStatus::Loading)
        {
            return;
        }
        self.revealing = None;
        if snapshot.target_status != Some(TargetStatus::Ready) {
            return;
        }
        let ancestors = snapshot.tree.ancestors(&id);
        self.expanded.extend(ancestors);
        self.sync_entries();
        let Some(index) = self.entries.iter().position(|e| e.row_id() == Some(id)) else {
            return;
        };
        self.list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    /// Opens a collapsed branch; its first reply comes into view.
    fn expand(&mut self, id: EventId, cx: &mut Context<Self>) {
        self.list.pause_following_tail();
        self.expanded.insert(id);
        self.sync_entries();
        self.reveal_child = self
            .snapshot
            .tree
            .children_of(&id)
            .first()
            .map(|row| row.id);
        cx.notify();
    }

    /// Targets a reply, or the root with `None`. Choosing the target again
    /// clears it.
    fn reply_to(&mut self, id: Option<EventId>, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_parent = match id {
            Some(id) if self.reply_parent == Some(id) => None,
            other => other,
        };
        self.input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.on_close)(window, cx);
    }

    fn send(&mut self, submission: Submission, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = &submission.editing {
            let current = self
                .snapshot
                .root
                .iter()
                .chain(self.snapshot.replies.iter())
                .find(|row| row.id == target.id)
                .cloned();
            self.message_actions.update(cx, |actions, cx| {
                actions.save_edit(submission, current, window, cx)
            });
            return;
        }
        let Some(root) = self.snapshot.root.as_ref().map(|root| root.id) else {
            self.composer.update(cx, |composer, cx| composer.failed(cx));
            return;
        };
        let parent = self.reply_parent;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        let mentions = submission.mentions.clone();
        // Keep the new reply in sight: open its branch, or follow the end
        // for a reply to the root.
        self.sent = Some(Sent {
            parent,
            known: self.snapshot.replies.iter().map(|reply| reply.id).collect(),
        });
        match parent {
            Some(parent) => {
                self.expanded.insert(parent);
                self.expanded.extend(self.snapshot.tree.ancestors(&parent));
                self.sync_entries();
            }
            None => self.list.set_follow_mode(FollowMode::Tail),
        }
        let thread = Some((root, parent.unwrap_or(root)));
        // Without files to upload, the intent is queued now, so the reply
        // shows on the next frame.
        let queued = submission.upload.is_none().then(|| {
            session.send(
                &channel,
                &submission.content,
                mentions.clone(),
                Vec::new(),
                thread,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = match queued {
                Some(queued) => queued.await,
                None => {
                    // Files upload in the background first; a failed
                    // upload is handed back to the composer's draft.
                    let Some(attachments) = submission.attachments().await else {
                        return;
                    };
                    session
                        .send(&channel, &submission.content, mentions, attachments, thread)
                        .await
                }
            };
            let handed_back = cx
                .update(|_, cx| submission.settle(&result, cx))
                .unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.error = None;
                        // A background send may land after the reader chose
                        // another target.
                        if parent.is_some() && this.reply_parent == parent {
                            this.reply_parent = None;
                        }
                        this.follow_sent();
                        this.composer
                            .update(cx, |composer, cx| composer.sent(window, cx));
                    }
                    Err(_) if handed_back => {}
                    Err(error) => {
                        this.sent = None;
                        this.error = Some(error.into());
                        this.composer.update(cx, |composer, cx| composer.failed(cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The reply target, if it is still among the loaded replies.
    fn selected_parent(&self) -> Option<&Arc<MessageRow>> {
        let id = self.reply_parent?;
        self.snapshot.replies.iter().find(|row| row.id == id)
    }

    fn can_send(&self) -> bool {
        (self.reply_parent.is_none() || self.selected_parent().is_some())
            && (!self.require_ready_root
                || (self.snapshot.status == ThreadStatus::Ready && self.snapshot.error.is_none()))
    }

    /// The loaded root is the one the draft names, ready to reply to.
    fn root_verified(&self) -> bool {
        !self.require_ready_root
            || (self.snapshot.root.as_ref().map(|root| root.id) == Some(self.root_id)
                && self.snapshot.target_status == Some(TargetStatus::Ready))
    }

    /// An older page failed while the root is shown: retry that page.
    fn older_page_status(&self) -> bool {
        let snapshot = &self.snapshot;
        snapshot.direction == Some(Direction::Older)
            && snapshot.read_kind == Some(ReadKind::Older)
            && snapshot.root.is_some()
    }

    fn retry_older(&mut self) {
        self.capture_older_anchor();
        self.view.load_more();
    }

    /// A channel known only from the cache shows its composer disabled,
    /// as buzz-app's does (a preview shows Join instead).
    fn composer_view(&self, _read_only: bool, _cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .child(self.composer.clone())
            .into_any_element()
    }

    /// Follows the root author's name into the placeholder, and the reply
    /// target into the composer's context box.
    fn sync_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let context = self.reply_parent.map(|_| match self.selected_parent() {
            Some(parent) => ReplyContext::Parent {
                name: super::workflows::sender_name(&profiles, parent),
                excerpt: parent.content.clone().into(),
            },
            None => ReplyContext::Missing,
        });
        // A channel known only from the cache cannot be written to yet.
        let read_only = self
            .community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
            .is_some_and(|channel| channel.read_only);
        let can_send = self.can_send() && !read_only;
        let placeholder = self.snapshot.root.as_ref().map(|root| {
            let name = super::workflows::sender_name(&profiles, root);
            reply_placeholder(root, &self.viewer, &name)
        });
        let root = self.snapshot.root.as_ref().map(|root| root.id);
        self.composer.update(cx, |composer, cx| {
            composer.set_context(context, cx);
            composer.set_can_send(can_send, cx);
            if let Some(placeholder) = placeholder {
                composer.set_placeholder(placeholder, window, cx);
            }
            // Plugins hear the thread's root, not the reply it opened at.
            if let Some(root) = root {
                composer.set_thread_root(root, window, cx);
            }
        });
    }

    /// Mounts the composer's tools where buzz-app mounts the composer:
    /// with its root known and verified, outside a preview, shown or not.
    pub fn mount_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.snapshot.root.as_ref().map(|root| root.id) {
            self.composer.update(cx, |composer, cx| {
                composer.set_thread_root(root, window, cx)
            });
        }
        let mounted = self.snapshot.root.is_some()
            && self.root_verified()
            && matches!(self.access(cx), Access::Member | Access::Other);
        if mounted {
            self.composer
                .update(cx, |composer, cx| composer.mount_tools(window, cx));
        }
    }

    /// After each layout: reports what the reader sees, follows the panel
    /// width, shows or hides the jump pill and follows unread replies.
    fn after_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let width = self.list.viewport_bounds().size.width;
        if width != self.width {
            let deeper = max_indent(width) != max_indent(self.width);
            self.width = width;
            if deeper {
                self.list.remeasure();
            }
            cx.notify();
        }
        if let Some(child) = self.reveal_child.take()
            && let Some(index) = self.entries.iter().position(|e| e.row_id() == Some(child))
        {
            self.list.scroll_to_reveal_item(index);
            cx.notify();
        }
        let entries = &self.entries;
        let view = reading::view(&self.list, &self.drawn, |ix| {
            entries.get(ix).and_then(Entry::row_id)
        });
        let input = self.input.read(cx).focus_handle(cx);
        let active = window.is_window_active()
            && (self.focus.contains_focused(window, cx) || input.contains_focused(window, cx));
        if let Some((visible, at_bottom)) = self.reporter.next(&view, active) {
            self.view.viewport(visible, at_bottom);
        }
        let away = reading::distance_from_bottom(&self.list) >= jump::DISTANCE;
        if self.jump.shown() != away {
            self.jump.set(away);
            cx.notify();
        }
        let replies: Vec<EventId> = self.snapshot.replies.iter().map(|reply| reply.id).collect();
        if self.watched != replies {
            self.unread_watch.set(Vec::new(), replies.clone());
            self.watched = replies;
        }
    }

    /// Keyboard scrolling of the focused thread history; Up, Page Up and
    /// Home also page in older replies near the top.
    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keys = &event.keystroke;
        let page = self.list.viewport_bounds().size.height * 0.875;
        let line = px(40.);
        match keys.key.as_str() {
            "up" => self.list.scroll_by(-line),
            "down" => self.list.scroll_by(line),
            "pageup" => self.list.scroll_by(-page),
            "pagedown" => self.list.scroll_by(page),
            "space" if keys.modifiers.shift => self.list.scroll_by(-page),
            "space" => self.list.scroll_by(page),
            "home" => self.list.scroll_to(ListOffset::default()),
            "end" => self.list.set_follow_mode(FollowMode::Tail),
            _ => return,
        }
        cx.stop_propagation();
        if self.list.logical_scroll_top().item_ix < LOAD_OLDER_WITHIN {
            self.load_older();
        }
        cx.notify();
    }
}

impl Render for ThreadPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // "Today" became "Yesterday": the day labels are rebuilt.
        if self.day != time::today() {
            self.sync_entries();
        }
        self.sync_composer(window, cx);
        let c = *cx.colors();
        let snapshot = self.snapshot.clone();
        if crate::perf::on() {
            crate::perf::shown(crate::perf::THREAD, "panel");
            if snapshot.root.is_some() {
                crate::perf::shown(crate::perf::THREAD, "root");
            }
            if !snapshot.replies.is_empty() {
                crate::perf::shown(crate::perf::THREAD, "replies");
            }
        }
        let owned = Rc::new(typing::owned_agents(cx));
        let narrow = window.viewport_size().width <= typing::NARROW;
        let community = self.community.read(cx);
        let profiles = community.snapshot.profiles.clone();
        let media = community.session.media().clone();
        let viewer = community.session.viewer().to_string();
        let read_only = community
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
            .is_some_and(|channel| channel.read_only);
        let access = self.access(cx);
        // A preview offers Join in place of the composer; once a join the
        // reader started makes them a member, they can reply.
        let preview = match &access {
            Access::Joinable(channel) | Access::Preview(channel) => Some(channel.clone()),
            Access::Member | Access::Other => None,
        };
        if self.joining && matches!(access, Access::Member) {
            self.joining = false;
            ThreadPanel::focus_composer(&cx.entity(), window, cx);
        }
        if let Some(channel) = &preview
            && self.join.is_none()
        {
            let community = self.community.clone();
            let join = cx.new(|_| JoinNotice::new(community, channel.clone()));
            self._subscriptions
                .push(cx.subscribe(&join, |this, _, _: &Joining, _| this.joining = true));
            self.join = Some(join);
        }
        let join = preview.as_ref().and(self.join.clone());
        if let Some(join) = &join {
            let ready = !self.entries.is_empty()
                || matches!(snapshot.status, ThreadStatus::Ready | ThreadStatus::Error);
            join.update(cx, |join, cx| join.set_ready(ready, cx));
        }
        let preview_inset = join.as_ref().map_or(px(0.), |join| join.read(cx).inset());
        let community = self.community.read(cx);
        let root_hex = snapshot
            .root
            .as_ref()
            .map_or_else(|| self.root_id.to_hex(), |root| root.id.to_hex());
        let mut activity = Activity::thread(
            community
                .snapshot
                .thread_typing
                .get(&root_hex)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            &root_hex,
            &owned,
        )
        .observe(crate::agent_activity::observer::observed(
            &self.community,
            &self.channel,
            Some(&root_hex),
            cx,
        ));
        // Message bubbles stand in for the composer's line
        // (`ActivityTypingReplacement`).
        activity.replace(&crate::agent_activity::request::bubbled_agents(
            &crate::agent_activity::observer::snapshot(&self.community, cx),
            &self.channel,
            Some(&root_hex),
        ));
        let members = community
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
            .map(|channel| channel.members.clone());
        let directory = Directory::of(community, members.as_deref(), cx);
        let names = directory.names.clone();
        self.activity.update(cx, |chooser, cx| {
            chooser.set(
                &activity,
                (&self.channel, Some(&root_hex)),
                (&profiles, &names, &media),
                |_| None,
                // Already here: the thread is open.
                Rc::new(|_, _, _| {}),
                cx,
            )
        });

        let weak = cx.entity().downgrade();
        let header = self.show_header.then(|| {
            PanelHeader::new().joined(true).leading(
                Tabs::new(TabVariant::Navigation).tab(
                    Tab::new("thread-tab", "Thread")
                        .icon(IconName::ChatCircle)
                        .selected(true)
                        .on_close(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.close(window, cx));
                        }),
                ),
            )
        });

        let note = |text: SharedString| {
            div()
                .px(px(24.))
                .py(px(8.))
                .text_caption()
                .text_color(c.text_subtle)
                .child(text)
        };
        let retry = |id: &'static str, older: bool, cx: &mut Context<Self>| {
            Button::new(id, "Retry thread")
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match older {
                        true => this.retry_older(),
                        false => this.view.refresh(),
                    }
                    cx.notify();
                }))
        };
        let older_page = self.older_page_status();

        // Seeded rows are not the thread yet: the loading mark holds its
        // place until this view's first page lands. Later pages and
        // repairs keep the rows showing.
        if self.presented.is_none() {
            let first_page = snapshot.root.is_some() && snapshot.direction.is_some();
            let settled = snapshot.status == ThreadStatus::Ready
                && !(snapshot.direction != Some(Direction::Older) && snapshot.can_load_more);
            if first_page || settled || snapshot.status == ThreadStatus::Error {
                self.presented = Some(std::time::Instant::now());
            }
        }
        let body = if self.presented.is_none() {
            self.loading(window, cx)
        } else if self.entries.is_empty() {
            div().flex_1().into_any_element()
        } else {
            let branch = Rc::new(Branch {
                media: media.clone(),
                owned: owned.clone(),
                unread: self.unread.unread.clone(),
                max: max_indent(self.width),
            });
            let mut rows = Rows::new(viewer.clone(), directory);
            rows.in_thread = true;
            let actions = self.message_actions.clone();
            MessageActions::fill(&actions, &mut rows, cx);
            rows.interaction.focused = super::message::keyboard_row(rows.in_thread, window);
            // A row the keyboard reaches scrolls into view, as focus does.
            if rows.interaction.focused != self.keyboard_row {
                self.keyboard_row = rows.interaction.focused;
                if let Some(index) = self.keyboard_row.and_then(|id| {
                    self.entries
                        .iter()
                        .position(|entry| entry.row_id() == Some(id))
                }) {
                    self.list.scroll_to_reveal_item(index);
                }
            }
            // Media hook: time chips seek the thread's one video.
            rows.media = super::media::thread_media(&self.snapshot, &rows.directory.media);
            let rows = Rc::new(rows);
            let entries = self.entries.clone();
            let drawn = self.drawn.clone();
            let weak = cx.entity().downgrade();
            let older_error = snapshot
                .error
                .clone()
                .filter(|_| older_page)
                .map(SharedString::from);
            let first_reply = entries
                .iter()
                .position(|entry| !matches!(entry, Entry::Root(_) | Entry::Missing));
            let narrow_panel = f32::from(self.width) > 0. && f32::from(self.width) <= 320.;
            let entry_count = entries.len();
            let after_layout = {
                let weak = weak.clone();
                canvas(
                    move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.after_layout(window, cx));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_0()
            };
            // Rows are drawn in layout, so they take the link mode with them.
            let link_mode = self.link_mode;
            let jump = self.jump.render(
                "thread-jump-to-latest",
                typing::has_agents(&self.activity, cx),
                preview_inset,
                cx.listener(|this, _, window, cx| {
                    this.list.set_follow_mode(FollowMode::Tail);
                    this.jump.set(false);
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
                cx,
            );
            // The rows fade in once they first show.
            let fade = Animation::new(duration::STATE)
                .with_easing(|t| easing::cubic_bezier(easing::STATE, t));
            div()
                .id("thread-history")
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if !this.focus.contains_focused(window, cx) {
                            window.focus(&this.focus, cx);
                        }
                    }),
                )
                .child(
                    list(self.list.clone(), move |index, _, cx| {
                        let Some(entry) = entries.get(index) else {
                            return div().into_any_element();
                        };
                        // The older page's error heads the replies.
                        let status = (Some(index) == first_reply)
                            .then(|| older_error.clone())
                            .flatten()
                            .map(|error| {
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.))
                                    .py(px(8.))
                                    .text_caption()
                                    .text_color(c.text_subtle)
                                    .child(error)
                                    .child({
                                        let weak = weak.clone();
                                        Button::new("retry-older", "Retry thread")
                                            .size(ButtonSize::Sm)
                                            .on_click(move |_, _, cx| {
                                                let _ = weak.update(cx, |this, cx| {
                                                    this.retry_older();
                                                    cx.notify();
                                                });
                                            })
                                    })
                            });
                        div()
                            .relative()
                            .pl(px(24.))
                            .pr(if narrow_panel { px(12.) } else { px(24.) })
                            .children(status)
                            .child(crate::ui::links::with_mode(link_mode, || {
                                render_entry(entry, &rows, &branch, &weak, cx)
                            }))
                            .child(drawn.probe(index))
                            // The last reply stays clear of a preview's Join.
                            .when(index + 1 == entry_count, |d| d.pb(preview_inset))
                            .into_any_element()
                    })
                    .flex_1()
                    .w_full(),
                )
                .child(after_layout)
                .children(jump)
                .with_animation(
                    ElementId::Name(format!("thread-rows-{}", cx.entity_id()).into()),
                    fade,
                    |rows, t| rows.opacity(t),
                )
                .into_any_element()
        };

        let mut status: Vec<AnyElement> = Vec::new();
        if self.require_ready_root
            && snapshot
                .root
                .as_ref()
                .is_some_and(|root| root.id != self.root_id)
        {
            status.push(
                note(
                    "This saved draft does not identify a thread root. Open its origin to check \
                     the destination."
                        .into(),
                )
                .into_any_element(),
            );
        }
        // A failed link selection notes itself instead of the target.
        let selection = self
            .selection_failure
            .as_ref()
            .filter(|_| snapshot.error.is_none());
        let unavailable =
            snapshot.target_status == Some(TargetStatus::Unavailable) && selection.is_none();
        if unavailable {
            status.push(note("Selected message unavailable.".into()).into_any_element());
        }
        let other_error = snapshot.error.clone().filter(|_| !older_page);
        if let Some(error) = &other_error {
            status.push(note(error.clone().into()).into_any_element());
        } else if snapshot.limited && snapshot.error.is_none() {
            status.push(note("Thread history limit reached.".into()).into_any_element());
        }
        if other_error.is_some() || unavailable {
            status.push(
                div()
                    .px(px(16.))
                    .pb(px(12.))
                    .child(retry("retry-thread", false, cx))
                    .into_any_element(),
            );
        }
        if let Some(failure) = selection {
            let text = match failure.unavailable {
                true => "That message is no longer available.",
                false => "Couldn’t open that message.",
            };
            status.push(note(text.into()).into_any_element());
            // Retrying cannot bring back a message that no longer exists.
            if !failure.unavailable {
                status.push(
                    div()
                        .px(px(16.))
                        .pb(px(12.))
                        .child(
                            Button::new("retry-selection", "Try again")
                                .size(ButtonSize::Sm)
                                .loading(failure.pending)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.retry_selection(window, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        // The older page's error shows with the replies; with none to show
        // it falls back here.
        if older_page
            && self.entries.len() <= 1
            && let Some(error) = &snapshot.error
        {
            status.push(note(error.clone().into()).into_any_element());
            status.push(
                div()
                    .px(px(16.))
                    .pb(px(12.))
                    .child(retry("retry-older-empty", true, cx))
                    .into_any_element(),
            );
        }

        let thread = super::thread_banners::BannerThread {
            relay: self.community.read(cx).origin.websocket().to_owned(),
            channel: self.channel.clone(),
            root: self.root_id.to_hex(),
        };
        for (index, banner) in super::thread_banners::for_thread(&thread, cx)
            .into_iter()
            .enumerate()
        {
            status.push(note(banner.text).into_any_element());
            if banner.buttons.is_empty() {
                continue;
            }
            let buttons = banner.buttons.into_iter().enumerate().map(|(at, button)| {
                let action = button.action;
                Button::new(
                    ElementId::Name(format!("thread-banner-{index}-{at}").into()),
                    button.label,
                )
                .size(ButtonSize::Sm)
                .loading(button.loading)
                .on_click(move |_, window, cx| action(window, cx))
            });
            status.push(
                div()
                    .px(px(16.))
                    .pb(px(12.))
                    .flex()
                    .gap(px(8.))
                    .children(buttons)
                    .into_any_element(),
            );
        }

        let typing = typing::overlay(
            &activity.humans,
            Some(self.activity.clone()),
            &names,
            narrow,
            cx,
        );

        // The panel itself takes focus, so Escape and the scrolling keys
        // work before its rows load.
        // The whole panel takes dropped files for the composer.
        super::composer::drop_zone(div(), &self.composer, cx)
            .id("thread-panel")
            .relative()
            .aria_label("Thread")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .text_body_sm()
            .text_color(c.text_standard)
            // The agent list closes on Escape before the composer sees it.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.activity.read(cx).is_open() {
                    this.activity.update(cx, |chooser, cx| chooser.close(cx));
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close(window, cx);
                } else if this.focus.is_focused(window) {
                    // Only the panel's own focus: keys in its composer edit.
                    this.key(event, cx);
                }
            }))
            .children(header)
            .child(match join {
                Some(join) => div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(body)
                    .child(join)
                    .into_any_element(),
                None => body,
            })
            .children(status)
            .child(div().h(px(12.)).flex_none())
            .children(self.error.clone().map(|error| {
                div()
                    .mx(px(16.))
                    .mb(px(8.))
                    .text_caption()
                    .text_color(c.text_warning)
                    .child(error)
            }))
            .when(
                preview.is_none() && snapshot.root.is_some() && self.root_verified(),
                |this| {
                    this.child(
                        div()
                            .relative()
                            .children(typing)
                            .child(self.composer_view(read_only, cx)),
                    )
                },
            )
            .children(super::composer::pane_drop_overlay(&self.composer, cx))
    }
}

/// Development only: `BUZZ_DEV_OPEN_THREAD=<event id>` opens that thread
/// beside the first channel selected, so headless runs can screenshot it.
pub fn dev_open_thread() -> Option<EventId> {
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if TAKEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    EventId::from_hex(&std::env::var("BUZZ_DEV_OPEN_THREAD").ok()?).ok()
}

/// One row of the flattened reply tree.
#[derive(Clone)]
enum Entry {
    Root(Item),
    /// The root is missing or unreadable.
    Missing,
    /// A top-level reply whose parent is not loaded.
    Orphan(EventId),
    Reply(Reply),
    Summary(Summary),
}

impl Entry {
    /// The message this row shows, which reading observes.
    fn row_id(&self) -> Option<EventId> {
        match self {
            Entry::Root(item) => Some(item.row.id),
            Entry::Reply(reply) => Some(reply.item.row.id),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Reply {
    item: Item,
    depth: usize,
    /// For each level `1..=depth`: the branch at that level continues
    /// below this row.
    rails: Vec<bool>,
    /// Children are shown, so a spine runs down from the avatar.
    open: bool,
}

/// A collapsed branch: "N replies" under its parent.
#[derive(Clone)]
struct Summary {
    parent: Arc<MessageRow>,
    depth: usize,
    rails: Vec<bool>,
    /// Every reply under the parent.
    descendants: Vec<EventId>,
    participants: Vec<String>,
}

impl Entry {
    /// Renders the same, so the list keeps its measurement.
    fn same(&self, other: &Self) -> bool {
        let same_item =
            |a: &Item, b: &Item| Arc::ptr_eq(&a.row, &b.row) && a.head == b.head && a.day == b.day;
        match (self, other) {
            (Self::Root(a), Self::Root(b)) => same_item(a, b),
            (Self::Missing, Self::Missing) => true,
            (Self::Orphan(a), Self::Orphan(b)) => a == b,
            (Self::Reply(a), Self::Reply(b)) => {
                same_item(&a.item, &b.item)
                    && a.depth == b.depth
                    && a.rails == b.rails
                    && a.open == b.open
            }
            (Self::Summary(a), Self::Summary(b)) => {
                Arc::ptr_eq(&a.parent, &b.parent)
                    && a.depth == b.depth
                    && a.rails == b.rails
                    && a.descendants == b.descendants
                    && a.participants == b.participants
            }
            _ => false,
        }
    }
}

/// Flattens the reply tree as buzz-app's `ThreadPanel` renders it: direct
/// replies in order, each nested branch collapsed behind a summary until
/// the reader opens it, one level at a time.
fn entries(snapshot: &ThreadSnapshot, expanded: &HashSet<EventId>) -> Vec<Entry> {
    let mut walk = Walk {
        tree: &snapshot.tree,
        expanded,
        root: snapshot.root.as_ref().map(|root| root.id),
        previous: snapshot.root.clone(),
        previous_parent: None,
        last_day: snapshot.root.as_ref().map(|root| root.created_at),
        out: Vec::new(),
    };
    match &snapshot.root {
        Some(root) => walk.out.push(Entry::Root(Item {
            row: root.clone(),
            head: true,
            day: Some(time::day(root.created_at).into()),
            group: None,
        })),
        None if matches!(snapshot.status, ThreadStatus::Ready | ThreadStatus::Error) => {
            walk.out.push(Entry::Missing)
        }
        None => {}
    }
    walk.branch(None, &snapshot.tree.top_level, 0, &mut Vec::new());
    walk.out
}

struct Walk<'a> {
    tree: &'a ReplyTree,
    expanded: &'a HashSet<EventId>,
    root: Option<EventId>,
    /// Groups only continue between siblings; a collapsed branch ends one.
    previous: Option<Arc<MessageRow>>,
    previous_parent: Option<EventId>,
    last_day: Option<u64>,
    out: Vec<Entry>,
}

impl Walk<'_> {
    fn branch(
        &mut self,
        parent: Option<EventId>,
        rows: &[Arc<MessageRow>],
        depth: usize,
        rails: &mut Vec<bool>,
    ) {
        for (index, row) in rows.iter().enumerate() {
            let last = index + 1 == rows.len();
            let children = self.tree.children_of(&row.id);
            let open = !children.is_empty() && self.expanded.contains(&row.id);
            if parent.is_none() && row.reply_parent_id.is_some_and(|p| Some(p) != self.root) {
                self.out.push(Entry::Orphan(row.id));
            }
            let continues = self.previous_parent == parent
                && self
                    .previous
                    .as_ref()
                    .is_some_and(|previous| message::continues(previous, row));
            let day = self
                .last_day
                .is_none_or(|day| !time::same_day(day, row.created_at))
                .then(|| time::day(row.created_at).into());
            if depth > 0 {
                rails.push(!last);
            }
            self.out.push(Entry::Reply(Reply {
                item: Item {
                    row: row.clone(),
                    head: !continues,
                    day,
                    group: None,
                },
                depth,
                rails: rails.clone(),
                open,
            }));
            self.previous = (children.is_empty() || open).then(|| row.clone());
            self.previous_parent = parent;
            self.last_day = Some(row.created_at);
            if open {
                self.branch(Some(row.id), children, depth + 1, rails);
            } else if !children.is_empty() {
                let mut descendants = Vec::new();
                self.collect(&row.id, &mut descendants);
                let mut participants: Vec<String> = Vec::new();
                for reply in &descendants {
                    let author = reply.author.to_hex();
                    if !participants.contains(&author) {
                        participants.push(author);
                    }
                }
                self.out.push(Entry::Summary(Summary {
                    parent: row.clone(),
                    depth,
                    rails: rails.clone(),
                    descendants: descendants.iter().map(|reply| reply.id).collect(),
                    participants,
                }));
            }
            if depth > 0 {
                rails.pop();
            }
        }
    }

    fn collect(&self, id: &EventId, out: &mut Vec<Arc<MessageRow>>) {
        for child in self.tree.children_of(id) {
            out.push(child.clone());
            self.collect(&child.id, out);
        }
    }
}

/// What reply rows share beyond [`Rows`].
struct Branch {
    media: Media,
    /// The viewer's agents, drawn as squircles.
    owned: Rc<HashSet<String>>,
    /// Replies that are unread, for "(N new)".
    unread: HashSet<EventId>,
    /// Indentation stops growing at this depth.
    max: usize,
}

impl Branch {
    fn indent(&self, depth: usize) -> f32 {
        INDENT * depth.min(self.max) as f32
    }

    /// A 1px connector segment at the spine of `level`'s parent.
    fn rail(&self, level: usize, color: Hsla) -> Div {
        div()
            .absolute()
            .left(px(self.indent(level - 1) + SPINE))
            .w(px(1.))
            .bg(color)
    }

    /// Pass-through lines for every branch that continues past this row.
    fn rails(&self, rails: &[bool], color: Hsla) -> Vec<Div> {
        rails
            .iter()
            .enumerate()
            .filter(|(index, continues)| **continues && *index < self.max)
            .map(|(index, _)| self.rail(index + 1, color).top_0().bottom_0())
            .collect()
    }
}

fn render_entry(
    entry: &Entry,
    rows: &Rc<Rows>,
    branch: &Rc<Branch>,
    panel: &WeakEntity<ThreadPanel>,
    cx: &mut App,
) -> AnyElement {
    let c = *cx.colors();
    match entry {
        Entry::Root(item) => message::render_in(Layout::Thread, item, rows, cx),
        Entry::Missing => div()
            .py(px(32.))
            .px(px(16.))
            .text_body_sm()
            .text_color(c.text_subtle)
            .child("Original message unavailable.")
            .into_any_element(),
        Entry::Orphan(_) => div()
            .pt(px(8.))
            .text_caption()
            .text_color(c.text_subtle)
            .child("Earlier reply unavailable in loaded history.")
            .into_any_element(),
        Entry::Reply(reply) => render_reply(reply, rows, branch, cx),
        Entry::Summary(summary) => {
            render_summary(summary, &rows.directory.profiles, branch, panel, cx)
        }
    }
}

fn render_reply(reply: &Reply, rows: &Rc<Rows>, branch: &Branch, cx: &mut App) -> AnyElement {
    let c = *cx.colors();
    let line = c.border_standard;
    let depth = reply.depth;
    let max = branch.max;
    let layout = if depth > 0 {
        Layout::Nested
    } else {
        Layout::Thread
    };
    // The day divider is drawn here so connectors can run through it.
    let mut divider = None;
    if let Some(day) = &reply.item.day {
        let mut lines = branch.rails(&reply.rails[..depth.saturating_sub(1)], line);
        if depth > 0 && depth <= max {
            lines.push(branch.rail(depth, line).top_0().bottom_0());
        }
        divider = Some(
            div()
                .relative()
                .pl(px(branch.indent(depth)))
                .children(lines)
                .child(message::day_divider(
                    day.clone(),
                    reply.item.row.created_at,
                    cx,
                )),
        );
    }
    let item = Item {
        day: None,
        ..reply.item.clone()
    };
    let top = if item.head { 14. } else { 2. };
    let avatar = if depth > 0 { 24. } else { 32. };
    let mut message_row = div()
        .relative()
        .pl(px(branch.indent(depth)))
        .children(branch.rails(&reply.rails[..depth.saturating_sub(1)], line));
    if depth > 0 && depth <= max {
        // The elbow into this row's avatar, and the parent's spine past it.
        message_row = message_row.child(
            branch
                .rail(depth, line)
                .top_0()
                .h(px(top + avatar / 2.))
                .w(px(INDENT - SPINE - 4.))
                .bg(transparent_black())
                .border_l_1()
                .border_b_1()
                .border_color(line)
                .rounded_bl(px(12.)),
        );
        if reply.rails[depth - 1] {
            message_row = message_row.child(branch.rail(depth, line).top_0().bottom_0());
        }
    }
    if reply.open && depth < max {
        message_row = message_row.child(
            branch
                .rail(depth + 1, line)
                .top(px(top + avatar + 4.))
                .bottom_0(),
        );
    }
    let message_row = message_row.child(message::render_in(layout, &item, rows, cx));
    div()
        .flex()
        .flex_col()
        .children(divider)
        .child(message_row)
        .into_any_element()
}

/// A collapsed branch (`ReplyBranch` + `ReplySummary`): a link-variant
/// small button with up to three avatars, "N replies" and "(N new)".
fn render_summary(
    summary: &Summary,
    profiles: &Profiles,
    branch: &Branch,
    panel: &WeakEntity<ThreadPanel>,
    cx: &mut App,
) -> AnyElement {
    let c = *cx.colors();
    let mut avatars = div().flex().items_center().pr(px(4.));
    for author in summary.participants.iter().take(3) {
        let profile = profiles.get(author);
        let mut avatar =
            Avatar::new(message::display_name(profiles, author)).size(AvatarSize::Px(21.));
        if profile.is_some_and(|p| p.is_agent) || branch.owned.contains(author) {
            avatar = avatar.squircle();
        }
        if let Some(picture) = profile.and_then(|p| p.picture.as_deref()) {
            avatar = avatar.image(media::avatar(picture, &branch.media));
        }
        avatars = avatars.child(
            div()
                .mr(px(-4.))
                .rounded_full()
                .p(px(1.5))
                .bg(c.surface_panel)
                .child(avatar),
        );
    }
    if summary.participants.len() > 3 {
        avatars = avatars.child(
            div()
                .mr(px(-4.))
                .size(px(24.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(c.affordance_selected)
                .text_caption()
                .text_color(c.text_subtle)
                .child(format!("+{}", summary.participants.len() - 3)),
        );
    }
    let count = summary.descendants.len();
    let label = if count == 1 {
        "1 reply".to_string()
    } else {
        format!("{count} replies")
    };
    let new = summary
        .descendants
        .iter()
        .filter(|id| branch.unread.contains(id))
        .count();
    let target = summary.parent.id;
    let panel = panel.clone();
    div()
        .relative()
        .pl(px(branch.indent(summary.depth) + 40.))
        .children(branch.rails(&summary.rails, c.border_standard))
        .child(
            div()
                .id(SharedString::from(format!(
                    "branch-{}",
                    summary.parent.id.to_hex()
                )))
                .flex()
                .flex_wrap()
                .items_center()
                .self_start()
                .gap(px(8.))
                .min_h(px(32.))
                .px(px(16.))
                .py(px(6.))
                .rounded(buzz_ui::tokens::radius::ICON_BUTTON)
                .cursor_pointer()
                .text_label_sm()
                .text_color(c.text_standard)
                .hover(|style| style.underline())
                .child(avatars)
                .child(label)
                .when(new > 0, |d| d.child(format!("({new} new)")))
                .on_click(move |_, _, cx| {
                    let _ = panel.update(cx, |this, cx| this.expand(target, cx));
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn the_reply_placeholder_never_addresses_the_viewer() {
        use nostr::prelude::Keys;
        let (viewer, other) = (Keys::generate().public_key(), Keys::generate().public_key());
        let root = |author, workflow_owner| MessageRow {
            author,
            workflow_owner,
            ..crate::ui::markdown::tests::row("Root")
        };
        let viewer_hex = viewer.to_hex();
        assert_eq!(
            reply_placeholder(&root(other, None), &viewer_hex, "Bob"),
            "Reply in thread to Bob"
        );
        assert_eq!(
            reply_placeholder(&root(viewer, None), &viewer_hex, "Alice"),
            "Reply in thread"
        );
        // A workflow the viewer owns, posted by its relay.
        assert_eq!(
            reply_placeholder(&root(other, Some(viewer)), &viewer_hex, "Deploy"),
            "Reply in thread"
        );
        assert_eq!(
            reply_placeholder(&root(viewer, Some(other)), &viewer_hex, "Deploy"),
            "Reply in thread to Deploy"
        );
    }

    #[test]
    fn narrow_panels_flatten_indentation_sooner() {
        assert_eq!(max_indent(px(300.)), 0);
        assert_eq!(max_indent(px(340.)), 3);
        assert_eq!(max_indent(px(420.)), 4);
        assert_eq!(max_indent(px(600.)), MAX_INDENT);
        // Before the first layout the panel is taken to be wide.
        assert_eq!(max_indent(px(0.)), MAX_INDENT);
    }
}
