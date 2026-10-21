//! What message rows ask of their page (`MessageActionBar.tsx`,
//! `MessageManagement.tsx`, `MessageReactions.tsx`, `ReportMessageDialog`):
//! the hover bar's popups and keyboard menu, reactions, copying, read
//! state, follows, "Send to channel", and the delete and report dialogs
//! with their outbox recovery. Channel pages and thread panels each own
//! one, so their rows behave the same.

use super::composer::{Composer, Submission};
use super::emoji_picker::{self, Dismissed, EmojiPicker, Pick, Picked, PickedAs};
use super::markdown::Directory;
use super::message::{Actions, Command, Hover, Interaction, MenuFacts, Popup, Rows};
use super::message_edit::{self, EditStatus, Save};
use super::outbox_ops::Ops;
use super::report::{ReportClosed, ReportDialog};
use super::{message_copy, notice, overlay};
use crate::community::Community;
use buzz_relay_client::fold::{CustomEmoji, MessageRow};
use buzz_relay_client::{Delivery, EventId, Outbox, SessionStatus};
use buzz_ui::{ActiveTheme, AlertDialog, Button, ButtonSize, ButtonVariant, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub type RowHook = Rc<dyn Fn(Arc<MessageRow>, &mut Window, &mut App)>;
pub type FocusHook = Rc<dyn Fn(&mut Window, &mut App)>;
pub type RootHook = Rc<dyn Fn(&App) -> Option<Arc<MessageRow>>>;
pub type FindHook = Rc<dyn Fn(EventId, &App) -> Option<Arc<MessageRow>>>;

/// What only the page knows how to do.
pub struct Hooks {
    /// Reply in the row's thread (opens or targets the thread).
    pub reply: RowHook,
    /// Opens the row's thread from its summary.
    pub open_thread: Option<RowHook>,
    /// Starts editing the row in the page's composer.
    pub edit: RowHook,
    pub focus_composer: FocusHook,
    /// The thread root as the panel shows it, for "Send to channel".
    pub thread_root: Option<RootHook>,
    /// A loaded row by id, for the picker's target.
    pub find: FindHook,
    /// Typing in this view: the channel, or a thread root.
    pub thread: Option<EventId>,
    /// The page's composer, where edits happen.
    pub composer: WeakEntity<Composer>,
}

/// An edit sent from the composer, followed until it lands.
struct Editing {
    target: Arc<MessageRow>,
    op: Option<EventId>,
}

struct Deletion {
    row: Arc<MessageRow>,
    /// Deleting an agent's message as its attested owner.
    agent: Option<String>,
    op: Option<EventId>,
    /// The removal was queued; its operation has not shown up yet.
    awaiting: bool,
    error: Option<SharedString>,
    focus: FocusHandle,
    /// Asked for by saving an empty edit: that edit (its composer
    /// generation) ends once the deletion lands.
    edit: Option<u64>,
}

/// A sent deletion an empty edit asked for. Only its receipt or echo ends
/// the edit and brings the draft back; a discarded deletion keeps the
/// correction (`MessageManagement`'s completions).
struct EditDeletion {
    row: EventId,
    op: Option<EventId>,
    edit: u64,
    /// The dialog that sent it: focus still there may go to the composer.
    dialog: FocusHandle,
}

pub struct MessageActions {
    community: Entity<Community>,
    channel: String,
    interaction: Interaction,
    picker: Entity<EmojiPicker>,
    menu: MenuFacts,
    menu_focus: FocusHandle,
    ops: Arc<Ops>,
    reaction_parents: HashMap<EventId, EventId>,
    outbox: Option<Outbox>,
    deleting: Option<Deletion>,
    edit_deletions: Vec<EditDeletion>,
    editing: Option<Editing>,
    reporting: Option<(Entity<ReportDialog>, Subscription)>,
    /// The conversation stopped being shown while a report was sent or
    /// failed: its row shows the state instead of the dialog until
    /// "Review report" (`ReportMessageDialog`'s `present`).
    report_hidden: bool,
    /// The page alert (`role="alert"`): unread and follow failures.
    error: Option<SharedString>,
    reaction_error: Option<(EventId, SharedString)>,
    hooks: Hooks,
    /// The rows' thread opener, made once: it lives as long as this place,
    /// which is how a plugin accessory's `openThread` knows it retired.
    open_thread: Option<crate::ui::message::RowHandler>,
    /// A reader's rows offer no Reply.
    no_reply: bool,
    status: Option<SessionStatus>,
    _outbox: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MessageActions {
    pub fn new(
        community: Entity<Community>,
        channel: String,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| EmojiPicker::new(community.clone(), window, cx).for_reactions());
        let subscriptions = vec![
            cx.subscribe_in(&picker, window, |this, _, Picked(pick), window, cx| {
                if let Some((id, Popup::Picker { from_bar })) = this.interaction.open.take()
                    && let Some(row) = (this.hooks.find)(id, cx)
                    && this.picker_shown(&row, from_bar, cx)
                {
                    let emoji = match pick {
                        Pick::Custom(custom) => Some(CustomEmoji {
                            shortcode: custom.shortcode.clone(),
                            url: custom.url.clone(),
                        }),
                        Pick::Unicode(_) => None,
                    };
                    this.react(row, pick.text(), emoji, window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &picker,
                window,
                |this, _, PickedAs(text, custom), window, cx| {
                    if let Some((id, Popup::Picker { from_bar })) = this.interaction.open.take()
                        && let Some(row) = (this.hooks.find)(id, cx)
                        && this.picker_shown(&row, from_bar, cx)
                    {
                        let emoji = CustomEmoji {
                            shortcode: custom.shortcode.clone(),
                            url: custom.url.clone(),
                        };
                        this.react(row, text.clone(), Some(emoji), window, cx);
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(&picker, window, |this, _, _: &Dismissed, window, cx| {
                // Escape gives focus back to the bar's "Add reaction".
                match this.interaction.open.take() {
                    Some((id, Popup::Picker { from_bar: true })) => {
                        let in_thread = this.hooks.thread.is_some();
                        super::message::focus_bar_button(in_thread, id, "add-reaction", window, cx);
                    }
                    _ => window.focus(&this.menu_focus, cx),
                }
                cx.notify();
            }),
            cx.observe_in(&community, window, |this, community, window, cx| {
                let status = community.read(cx).snapshot.status.clone();
                if this.status.as_ref() != Some(&status) {
                    this.status = Some(status.clone());
                    if status == SessionStatus::Ready {
                        this.watch_outbox(cx);
                    }
                }
                // The dialog goes when its conversation can no longer change,
                // and so does the reaction picker.
                if this.deleting.is_some() && !this.writable(cx) {
                    this.deleting = None;
                    (this.hooks.focus_composer)(window, cx);
                }
                if matches!(this.interaction.open, Some((_, Popup::Picker { .. })))
                    && !this.writable(cx)
                {
                    this.interaction.open = None;
                }
                cx.notify();
            }),
        ];
        let mut this = Self {
            community,
            channel,
            interaction: Interaction::default(),
            picker,
            menu: MenuFacts::default(),
            menu_focus: cx.focus_handle(),
            ops: Arc::new(Ops::default()),
            reaction_parents: HashMap::new(),
            outbox: None,
            deleting: None,
            edit_deletions: Vec::new(),
            editing: None,
            reporting: None,
            report_hidden: false,
            error: None,
            reaction_error: None,
            no_reply: false,
            open_thread: hooks.open_thread.clone().map(|open| {
                Rc::new(
                    move |row: &Arc<MessageRow>, window: &mut Window, cx: &mut App| {
                        open(row.clone(), window, cx)
                    },
                ) as crate::ui::message::RowHandler
            }),
            hooks,
            status: None,
            _outbox: None,
            _subscriptions: subscriptions,
        };
        this.watch_outbox(cx);
        this
    }

    fn session(&self, cx: &App) -> Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    fn summary(&self, cx: &App) -> Option<buzz_relay_client::ChannelSummary> {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel)
            .cloned()
    }

    /// Rows here open no thread (a reader's).
    pub fn without_threads(&mut self, cx: &mut Context<Self>) {
        self.open_thread = None;
        self.no_reply = true;
        cx.notify();
    }

    /// The reaction picker is on screen: its conversation takes
    /// reactions, and the inline "+" shows only beside reactions
    /// (`MessageReactionControls`, `ReactionTool`).
    fn picker_shown(&self, row: &MessageRow, from_bar: bool, cx: &App) -> bool {
        self.writable(cx) && (from_bar || !row.reactions.is_empty())
    }

    fn writable(&self, cx: &App) -> bool {
        self.summary(cx)
            .is_some_and(|c| !c.archived && !c.read_only)
    }

    /// Follows the outbox, so rows see edits, deletions and reactions in
    /// flight. A reconnect brings a new outbox.
    fn watch_outbox(&mut self, cx: &mut Context<Self>) {
        let session = self.session(cx);
        self._outbox = Some(cx.spawn(async move |this, cx| {
            let outbox = loop {
                if let Some(outbox) = session.outbox().await {
                    break outbox;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
            };
            let mut changes = outbox.changes();
            if this
                .update(cx, |this, cx| {
                    this.outbox = Some(outbox.clone());
                    this.read_ops(cx);
                })
                .is_err()
            {
                return;
            }
            while changes.changed().await.is_ok() {
                if this.update(cx, |this, cx| this.read_ops(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn read_ops(&mut self, cx: &mut Context<Self>) {
        let Some(outbox) = &self.outbox else {
            return;
        };
        self.ops = Arc::new(Ops::read(outbox, self.reaction_parents.clone()));
        // The delete dialog follows its operation until it lands.
        let mut close = false;
        if let Some(deletion) = &mut self.deleting {
            if deletion.op.is_none()
                && deletion.awaiting
                && let Some(op) = self.ops.deletion(deletion.row.id)
            {
                deletion.op = Some(op.id);
                deletion.awaiting = false;
            }
            if let Some(id) = deletion.op {
                close = self
                    .ops
                    .get(id)
                    .is_none_or(|op| matches!(op.delivery, Delivery::Seen | Delivery::Accepted));
            }
        }
        let landed = self.settle_edit_deletions();
        if close || !landed.is_empty() {
            // The dialog was open, so focus goes back to the composer.
            let dialog = close && self.deleting.take().is_some();
            let focus = self.hooks.focus_composer.clone();
            let composer = self.hooks.composer.clone();
            cx.defer(move |cx| {
                if let Some(window) = cx.active_window() {
                    let _ = window.update(cx, |_, window, cx| {
                        let mut refocus = dialog;
                        for landed in &landed {
                            refocus |= end_edit(&composer, landed, window, cx);
                        }
                        if refocus {
                            focus(window, cx)
                        }
                    });
                }
            });
        }
        self.follow_edit(cx);
        cx.notify();
    }

    /// Saves the composer's edit (`useMessageEdit`'s `save`): checks it,
    /// sends it through the outbox, and keeps the composer locked until it
    /// lands; an empty edit asks to delete the message. `current` is the
    /// target row as the page has it now: the page is mid-update when its
    /// composer sends, so the `find` hook can't read it here.
    pub fn save_edit(
        &mut self,
        submission: Submission,
        current: Option<Arc<MessageRow>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = submission.editing.clone() else {
            return;
        };
        let Some(composer) = self.hooks.composer.upgrade() else {
            return;
        };
        let initial = composer
            .read(cx)
            .edit_initial()
            .map(str::to_string)
            .unwrap_or_default();
        let editable = current.as_ref().is_some_and(|row| {
            super::message::editable(row) && !self.ops.deleting(row.id) && self.writable(cx)
        });
        match message_edit::plan(
            &target,
            current.as_deref(),
            editable,
            &initial,
            &submission.content,
        ) {
            Save::Close => composer.update(cx, |composer, cx| composer.sent(window, cx)),
            Save::Delete => {
                composer.update(cx, |composer, cx| composer.set_notice(None, cx));
                let edit = composer.read(cx).edit_session().map(|(_, edit)| edit);
                self.open_delete(target, window, cx);
                if let Some(deletion) = &mut self.deleting {
                    deletion.edit = edit;
                }
            }
            Save::Error(error) => composer.update(cx, |composer, cx| {
                composer.set_notice(Some(error.into()), cx)
            }),
            Save::Send(body) => {
                self.editing = Some(Editing {
                    target: target.clone(),
                    op: None,
                });
                composer.update(cx, |composer, cx| {
                    composer.set_edit_status(
                        EditStatus {
                            locked: true,
                            busy: true,
                            retryable: false,
                        },
                        cx,
                    )
                });
                let (session, channel) = (self.session(cx), self.channel.clone());
                cx.spawn(async move |this, cx| {
                    let result = session.edit(&channel, target.id, &body).await;
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(()) => this.read_ops(cx),
                        Err(error) => {
                            this.editing = None;
                            if let Some(composer) = this.hooks.composer.upgrade() {
                                composer.update(cx, |composer, cx| {
                                    composer.set_edit_status(EditStatus::default(), cx);
                                    composer.set_notice(Some(error.into()), cx);
                                });
                            }
                        }
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// "Retry edit": republishes the edit still in the outbox.
    pub fn retry_edit(&mut self, cx: &mut Context<Self>) {
        let Some(op) = self.editing.as_ref().and_then(|e| e.op) else {
            return;
        };
        let session = self.session(cx);
        cx.foreground_executor()
            .spawn(async move {
                let _ = session.retry_send(op).await;
            })
            .detach();
    }

    /// Mirrors the sent edit's delivery into the composer, and ends the
    /// edit once the relay has it.
    fn follow_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        if editing.op.is_none() {
            editing.op = self
                .ops
                .list
                .iter()
                .rev()
                .find(|op| {
                    op.kind == buzz_relay_client::kinds::EDIT
                        && op.target == Some(editing.target.id)
                })
                .map(|op| op.id);
        }
        let Some(id) = editing.op else {
            return;
        };
        let Some(composer) = self.hooks.composer.upgrade() else {
            return;
        };
        match self.ops.get(id) {
            Some(op) if !matches!(op.delivery, Delivery::Seen | Delivery::Accepted) => {
                let retryable = matches!(op.delivery, Delivery::Failed | Delivery::Unknown);
                let error = op.error.clone();
                composer.update(cx, |composer, cx| {
                    composer.set_edit_status(
                        EditStatus {
                            locked: true,
                            busy: op.delivery == Delivery::Sending,
                            retryable,
                        },
                        cx,
                    );
                    if retryable && error.is_some() {
                        composer.set_notice(error.map(Into::into), cx);
                    }
                });
            }
            _ => {
                self.editing = None;
                cx.defer(move |cx| {
                    if let Some(window) = cx.active_window() {
                        let _ = window.update(cx, |_, window, cx| {
                            composer.update(cx, |composer, cx| {
                                composer.set_edit_status(EditStatus::default(), cx);
                                composer.sent(window, cx)
                            })
                        });
                    }
                });
            }
        }
    }

    /// Fills a page's rows with these actions and their state.
    /// The conversation is shown or not (`ConversationPresentation`). An
    /// unsent report is discarded as it hides; a sent or failed one stays,
    /// summed up in its row.
    /// A delete confirmation goes too; a deletion it already sent still
    /// completes.
    pub fn set_presented(&mut self, presented: bool, cx: &mut Context<Self>) {
        if presented {
            return;
        }
        if self.deleting.take().is_some() {
            cx.notify();
        }
        let Some((dialog, _)) = &self.reporting else {
            return;
        };
        let dialog = dialog.read(cx);
        if dialog.pending() || dialog.error().is_some() {
            self.report_hidden = true;
        } else {
            self.reporting = None;
        }
        cx.notify();
    }

    pub fn fill(this: &Entity<Self>, rows: &mut Rows, cx: &mut App) {
        let quick = emoji_picker::quick_bar(&this.read(cx).community.clone(), cx);
        let me = this.read(cx);
        me.fill_rows(rows, this, quick, cx);
    }

    fn fill_rows(
        &self,
        rows: &mut Rows,
        this: &Entity<Self>,
        quick: Vec<emoji_picker::QuickReaction>,
        cx: &App,
    ) {
        let community = self.community.read(cx);
        let snapshot = community.snapshot.clone();
        rows.channel = self.summary(cx);
        rows.interaction = self.interaction.clone();
        rows.picker = Some(self.picker.clone());
        rows.ops = self.ops.clone();
        rows.menu = self.menu;
        rows.menu_focus = Some(self.menu_focus.clone());
        rows.reaction_error = self.reaction_error.clone();
        rows.report = self
            .reporting
            .as_ref()
            .filter(|_| self.report_hidden)
            .map(|(dialog, _)| {
                let dialog = dialog.read(cx);
                (dialog.message(), dialog.error())
            });
        rows.presence = snapshot.presence.clone();
        rows.bubbles = Some(crate::ui::agent_activity::bubble::Bubbles::of(
            &self.community,
            &self.channel,
            cx,
        ));
        rows.quick_reactions = quick;
        rows.actions = self.actions(this);
    }

    fn actions(&self, this: &Entity<Self>) -> Actions {
        let weak = this.downgrade();
        let (hover, popup, command, highlight, preview, profiles, retry) = (
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak,
        );
        Actions {
            on_open_thread: self.open_thread.clone(),
            find_row: Some(self.hooks.find.clone()),
            no_reply: self.no_reply,
            on_retry: Some(Rc::new(move |row, _, cx| {
                let _ = retry.update(cx, |this, cx| {
                    let (session, id) = (this.session(cx), row.id);
                    cx.foreground_executor()
                        .spawn(async move {
                            let _ = session.retry_send(id).await;
                        })
                        .detach();
                });
            })),
            on_hover: Some(Rc::new(move |id, target, hovered, _, cx| {
                let _ = hover.update(cx, |this, cx| this.hover(id, target, hovered, cx));
            })),
            on_popup: Some(Rc::new(move |open, window, cx| {
                let _ = popup.update(cx, |this, cx| this.set_popup(open, window, cx));
            })),
            on_command: Some(Rc::new(move |row, action, window, cx| {
                let row = row.clone();
                let _ = command.update(cx, |this, cx| this.command(row, action, window, cx));
            })),
            on_highlight: Some(Rc::new(move |index, _, cx| {
                let _ = highlight.update(cx, |this, cx| {
                    this.interaction.highlight = index;
                    cx.notify();
                });
            })),
            on_preview: Some(Rc::new(move |open, _, cx| {
                let _ = preview.update(cx, |this, cx| {
                    if this.interaction.preview != open {
                        this.interaction.preview = open;
                        cx.notify();
                    }
                });
            })),
            on_want_profiles: Some(Rc::new(move |people, cx| {
                let _ = profiles.update(cx, |this, cx| {
                    let known = this.community.read(cx).snapshot.profiles.clone();
                    let missing: Vec<String> = people
                        .into_iter()
                        .filter(|p| !known.contains_key(p))
                        .collect();
                    if !missing.is_empty() {
                        this.session(cx).want_profiles(missing);
                    }
                });
            })),
        }
    }

    fn hover(&mut self, id: EventId, target: Hover, hovered: bool, cx: &mut Context<Self>) {
        // A row under the pointer means it left every other row's bar; a bar
        // that vanished while hovered never reports leaving.
        if target == Hover::Row
            && hovered
            && self.interaction.bar_hovered.is_some_and(|bar| bar != id)
        {
            self.interaction.bar_hovered = None;
            cx.notify();
        }
        let slot = match target {
            Hover::Row => &mut self.interaction.hovered,
            Hover::Bar => &mut self.interaction.bar_hovered,
        };
        let next = if hovered {
            Some(id)
        } else if *slot == Some(id) {
            None
        } else {
            *slot
        };
        if *slot != next {
            *slot = next;
            cx.notify();
        }
    }

    fn set_popup(
        &mut self,
        open: Option<(EventId, Popup)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.interaction.open == open {
            return;
        }
        match open {
            Some((_, Popup::Picker { .. })) => {
                self.picker.update(cx, |picker, cx| {
                    picker.reset(window, cx);
                    picker.focus(window, cx);
                });
            }
            Some((id, Popup::More)) => {
                self.menu = MenuFacts::default();
                self.interaction.highlight = None;
                window.focus(&self.menu_focus, cx);
                self.read_menu_facts(id, cx);
            }
            _ => {}
        }
        self.interaction.open = open;
        cx.notify();
    }

    /// The more menu's read and follow choices depend on state only the
    /// session holds.
    fn read_menu_facts(&mut self, id: EventId, cx: &mut Context<Self>) {
        let Some(row) = (self.hooks.find)(id, cx) else {
            return;
        };
        let (session, channel) = (self.session(cx), self.channel.clone());
        let root = row.thread_root_id.unwrap_or(row.id);
        cx.spawn(async move |this, cx| {
            let unread = session.message_unread(&channel, id).await;
            let following = session.following(&channel, root).await;
            let _ = this.update(cx, |this, cx| {
                if this.interaction.open == Some((id, Popup::More)) {
                    this.menu = MenuFacts { unread, following };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn command(
        &mut self,
        row: Arc<MessageRow>,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (session, channel) = (self.session(cx), self.channel.clone());
        match command {
            Command::CopyLink => {
                if let Some(link) = message_copy::link(&row) {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                    notice::success("Link copied", cx);
                }
            }
            Command::CopyMessage => {
                let community = self.community.read(cx);
                let directory = Directory::of(community, None, cx);
                let text = message_copy::text(&row, &directory.profiles, &directory.names);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                notice::success("Message copied", cx);
            }
            Command::MarkRead | Command::MarkUnread => {
                self.error = None;
                let read = command == Command::MarkRead;
                cx.spawn(async move |this, cx| {
                    let result = if read {
                        session.mark_message_read(&channel, row.id).await
                    } else {
                        session.mark_message_unread(&channel, row.id).await
                    };
                    if result.is_err() {
                        let _ = this.update(cx, |this, cx| {
                            this.error = Some("Could not update unread state. Try again.".into());
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            Command::SendToChannel => {
                let root = self.hooks.thread_root.as_ref().and_then(|root| root(cx));
                cx.spawn(async move |_, cx| {
                    let result = session.send_to_channel(row, root).await;
                    cx.update(|cx| match result {
                        Ok(()) => notice::success("Sending to channel", cx),
                        Err(error) => notice::error(error, cx),
                    });
                })
                .detach();
            }
            Command::Edit => (self.hooks.edit)(row, window, cx),
            Command::Delete => self.open_delete(row, window, cx),
            Command::ViewActivity => {
                if let Some(target) =
                    crate::ui::profile_panel::activity_target(&row.author.to_hex())
                {
                    crate::ui::navigation::open_url(&target, window, cx);
                }
            }
            Command::Follow(follow) => {
                self.error = None;
                let root = row.thread_root_id.unwrap_or(row.id);
                cx.spawn(async move |this, cx| {
                    if session.follow(&channel, root, follow).await.is_err() {
                        let _ = this.update(cx, |this, cx| {
                            this.error = Some("Could not save thread follow. Try again.".into());
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            Command::Report => {
                let dialog = cx.new(|cx| {
                    ReportDialog::new(session.clone(), channel.clone(), row.id, window, cx)
                });
                let subscription = cx.subscribe(&dialog, |this, _, ReportClosed, cx| {
                    this.reporting = None;
                    this.report_hidden = false;
                    cx.notify();
                });
                self.reporting = Some((dialog, subscription));
                self.report_hidden = false;
            }
            Command::ReviewReport => {
                self.report_hidden = false;
                cx.notify();
            }
            Command::Reply => (self.hooks.reply)(row, window, cx),
            Command::React { content, emoji } => self.react(row, content, emoji, window, cx),
            Command::Action(key) => super::renderers::run_action(&key, &row, window, cx),
            Command::RetryOperation(id) => {
                cx.foreground_executor()
                    .spawn(async move {
                        let _ = session.retry_send(id).await;
                    })
                    .detach();
            }
        }
        cx.notify();
    }

    /// Toggles the viewer's reaction (`useReactionAction`): adds it, or
    /// removes every one of theirs in that group. Locked while a change
    /// for this message is still in the outbox.
    fn react(
        &mut self,
        row: Arc<MessageRow>,
        content: String,
        emoji: Option<CustomEmoji>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ops.reaction_blocked(&row) || !self.writable(cx) {
            return;
        }
        let (session, channel) = (self.session(cx), self.channel.clone());
        let viewer = session.viewer().to_string();
        let mine: Vec<EventId> = row
            .reactions
            .iter()
            .find(|group| {
                group.content == content
                    && group.emoji.as_ref().map(|e| &e.url) == emoji.as_ref().map(|e| &e.url)
            })
            .map(|group| {
                group
                    .events
                    .iter()
                    .filter(|r| r.author.to_hex() == viewer)
                    .map(|r| r.id)
                    .collect()
            })
            .unwrap_or_default();
        self.reaction_error = None;
        if mine.is_empty() {
            let partition = self.community.read(cx).partition();
            emoji_picker::record_reaction(&partition, &content, cx);
        } else {
            for id in &mine {
                self.reaction_parents.insert(*id, row.id);
            }
        }
        let target = row.id;
        cx.spawn(async move |this, cx| {
            let result = if mine.is_empty() {
                let custom = emoji.map(|e| buzz_relay_client::CustomEmoji {
                    shortcode: e.shortcode,
                    url: e.url,
                });
                session.react_with(&channel, target, &content, custom).await
            } else {
                let mut result = Ok(());
                for id in mine {
                    if let Err(error) = session.remove(&channel, id).await {
                        result = Err(error);
                    }
                }
                result
            };
            if result.is_err() {
                let _ = this.update(cx, |this, cx| {
                    this.reaction_error =
                        Some((target, "Could not update reaction. Try again.".into()));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_delete(&mut self, row: Arc<MessageRow>, window: &mut Window, cx: &mut Context<Self>) {
        let viewer = self.session(cx).viewer().to_string();
        let author = row.author.to_hex();
        let agent = (author != viewer).then_some(author);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.deleting = Some(Deletion {
            op: self.ops.deletion(row.id).map(|op| op.id),
            row,
            agent,
            awaiting: false,
            error: None,
            focus,
            edit: None,
        });
    }

    fn run_delete(&mut self, cx: &mut Context<Self>) {
        let writable = self.writable(cx);
        let session = self.session(cx);
        let channel = self.channel.clone();
        let Some(deletion) = &mut self.deleting else {
            return;
        };
        deletion.error = None;
        if !writable {
            deletion.error = Some("This conversation is no longer available for changes.".into());
            cx.notify();
            return;
        }
        if let Some(edit) = deletion.edit
            && !self
                .edit_deletions
                .iter()
                .any(|pending| pending.row == deletion.row.id)
        {
            self.edit_deletions.push(EditDeletion {
                row: deletion.row.id,
                op: deletion.op,
                edit,
                dialog: deletion.focus.clone(),
            });
        }
        if let Some(op) = deletion.op {
            cx.foreground_executor()
                .spawn(async move {
                    let _ = session.retry_send(op).await;
                })
                .detach();
            return;
        }
        deletion.awaiting = true;
        let (id, agent) = (deletion.row.id, deletion.agent.clone());
        cx.spawn(async move |this, cx| {
            let result = match agent {
                Some(agent) => session.remove_as_owner(&channel, id, &agent).await,
                None => session.remove(&channel, id).await,
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(deletion) = &mut this.deleting {
                    match result {
                        Err(error) => {
                            deletion.awaiting = false;
                            deletion.error = Some(error.into());
                            this.edit_deletions
                                .retain(|pending| pending.row != id || pending.op.is_some());
                        }
                        Ok(()) => this.read_ops(cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Matches empty-edit deletions to their operations; returns those that
    /// landed and forgets those discarded from the outbox.
    fn settle_edit_deletions(&mut self) -> Vec<EditDeletion> {
        let ops = &self.ops;
        let mut landed = Vec::new();
        self.edit_deletions.retain_mut(|pending| {
            if pending.op.is_none() {
                pending.op = ops.deletion(pending.row).map(|op| op.id);
            }
            let Some(op) = pending.op else {
                return true;
            };
            match ops.get(op).map(|op| op.delivery) {
                Some(Delivery::Seen | Delivery::Accepted) => {
                    landed.push(EditDeletion {
                        row: pending.row,
                        op: pending.op,
                        edit: pending.edit,
                        dialog: pending.dialog.clone(),
                    });
                    false
                }
                Some(_) => true,
                None => false,
            }
        });
        landed
    }

    fn close_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .deleting
            .as_ref()
            .is_some_and(|d| self.delete_pending(d));
        if pending {
            return;
        }
        let finished = self.deleting.take().is_some_and(|d| d.op.is_some());
        if finished {
            (self.hooks.focus_composer)(window, cx);
        }
        cx.notify();
    }

    fn delete_pending(&self, deletion: &Deletion) -> bool {
        deletion.awaiting
            || deletion
                .op
                .and_then(|id| self.ops.get(id))
                .is_some_and(|op| op.delivery == Delivery::Sending)
    }

    /// The delete and report dialogs and the toast, drawn over the page.
    pub fn overlays(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(deletion) = &self.deleting {
            out.push(self.delete_dialog(deletion, window, cx));
        }
        if let Some((dialog, _)) = self.reporting.as_ref().filter(|_| !self.report_hidden) {
            let close = dialog.clone();
            out.push(
                overlay::modal(
                    window,
                    dialog.clone(),
                    Some(Rc::new(move |_, cx| {
                        close.update(cx, |dialog, cx| {
                            if !dialog.pending() {
                                cx.emit(ReportClosed);
                            }
                        })
                    })),
                )
                .into_any_element(),
            );
        }
        out.extend(notice::render(window, cx));
        out
    }

    fn delete_dialog(
        &self,
        deletion: &Deletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let op = deletion.op.and_then(|id| self.ops.get(id));
        let pending = self.delete_pending(deletion);
        let retryable =
            op.is_some_and(|op| matches!(op.delivery, Delivery::Failed | Delivery::Unknown));
        let alert = deletion.error.clone().or_else(|| {
            retryable.then(|| {
                op.and_then(|op| op.error.clone())
                    .unwrap_or_else(|| {
                        "Delivery could not be confirmed. Retry sends the same operation.".into()
                    })
                    .into()
            })
        });
        let label = if pending {
            "Deleting…"
        } else if retryable {
            "Retry"
        } else {
            "Delete"
        };
        let this = cx.entity().downgrade();
        let dialog = div()
            .track_focus(&deletion.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_delete(window, cx);
                }
            }))
            .child(
                AlertDialog::new("Delete message?")
                    .description(
                        "This requests removal of this message from Buzz’s relay. People may \
                         still have copies. This cannot be undone.",
                    )
                    .when_some(alert, |d, alert| {
                        d.child(div().text_body_sm().text_color(c.text_danger).child(alert))
                    })
                    .action(
                        Button::new(
                            "delete-cancel",
                            if deletion.op.is_some() {
                                "Close"
                            } else {
                                "Cancel"
                            },
                        )
                        .variant(ButtonVariant::Subtle)
                        .size(ButtonSize::Sm)
                        .disabled(pending)
                        .on_click(cx.listener(|this, _, window, cx| this.close_delete(window, cx))),
                    )
                    .action(
                        Button::new("delete-confirm", label)
                            .variant(ButtonVariant::Destructive)
                            .size(ButtonSize::Sm)
                            .disabled(pending)
                            .on_click(cx.listener(|this, _, _, cx| this.run_delete(cx))),
                    ),
            );
        overlay::modal(
            window,
            dialog,
            Some(Rc::new(move |window, cx| {
                let _ = this.update(cx, |this, cx| this.close_delete(window, cx));
            })),
        )
        .into_any_element()
    }

    /// Above the composer: the page alert, and failed or unconfirmed edits
    /// and deletions with Retry and Discard (`MessageManagementStatus`).
    pub fn status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = *cx.colors();
        let recovery = self.ops.recovery(&self.channel);
        if recovery.is_empty() && self.error.is_none() {
            return None;
        }
        let writable = self.writable(cx);
        let mut column = div().flex().flex_col().gap(px(8.)).mx(px(16.)).mb(px(8.));
        for op in recovery {
            let (retry_id, dismiss_id) = (op.id, op.id);
            let edit = op.kind == buzz_relay_client::kinds::EDIT;
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(12.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(c.border_standard)
                    .text_body_sm()
                    .text_color(c.text_standard)
                    .child(format!(
                        "{}: {}",
                        if edit {
                            "Message edit"
                        } else {
                            "Message deletion"
                        },
                        op.error
                            .clone()
                            .unwrap_or_else(|| "Delivery could not be confirmed.".into())
                    ))
                    .when(edit, |d| {
                        d.child(div().text_color(c.text_subtle).child(op.content.clone()))
                    })
                    .when(op.delivery == Delivery::Unknown, |d| {
                        d.child(div().text_color(c.text_subtle).child(
                            "This may already have reached the conversation. Retry sends the \
                             same operation; dismissing does not undo it.",
                        ))
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new(
                                    SharedString::from(format!("retry-update-{}", op.id.to_hex())),
                                    "Retry message update",
                                )
                                .size(ButtonSize::Sm)
                                .disabled(!writable)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let session = this.session(cx);
                                        cx.foreground_executor()
                                            .spawn(async move {
                                                let _ = session.retry_send(retry_id).await;
                                            })
                                            .detach();
                                    },
                                )),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!(
                                        "dismiss-update-{}",
                                        op.id.to_hex()
                                    )),
                                    if op.delivery == Delivery::Failed {
                                        "Discard failed update"
                                    } else {
                                        "Dismiss unconfirmed update"
                                    },
                                )
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let session = this.session(cx);
                                        cx.spawn(async move |this, cx| {
                                            if let Err(error) = session.dismiss(dismiss_id).await {
                                                let _ = this.update(cx, |this, cx| {
                                                    this.error = Some(error.into());
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .detach();
                                    },
                                )),
                            ),
                    ),
            );
        }
        if let Some(error) = &self.error {
            column = column.child(
                div()
                    .text_caption()
                    .text_color(c.text_warning)
                    .child(error.clone()),
            );
        }
        Some(column.into_any_element())
    }

    /// Up in an empty composer edits the viewer's newest editable message
    /// (`lastEditableMessage`): own, delivered, ordinary, with no edit or
    /// deletion of it in the outbox.
    pub fn last_editable<'a>(
        &self,
        rows: impl DoubleEndedIterator<Item = &'a Arc<MessageRow>>,
        cx: &App,
    ) -> Option<Arc<MessageRow>> {
        let viewer = self.session(cx).viewer().to_string();
        let archived = self.summary(cx).is_none_or(|c| c.archived || c.read_only);
        if archived {
            return None;
        }
        rows.rev()
            .find(|row| {
                row.author.to_hex() == viewer
                    && super::message::editable(row)
                    && !self.ops.deleting(row.id)
                    && !self.ops.editing(row.id)
            })
            .cloned()
    }
}

/// Ends the edit a landed deletion was asked for, bringing back the draft
/// it set aside, unless another edit replaced it. Returns whether focus
/// may go to the composer: it was nowhere, in the composer or in the
/// dialog, never in another control.
fn end_edit(
    composer: &WeakEntity<Composer>,
    landed: &EditDeletion,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    composer
        .update(cx, |composer, cx| {
            if composer.edit_session() != Some((landed.row, landed.edit)) {
                return false;
            }
            let focus = window.focused(cx).is_none()
                || composer.input_focused(window, cx)
                || landed.dialog.is_focused(window);
            composer.set_edit_status(EditStatus::default(), cx);
            composer.cancel_edit(window, cx);
            focus
        })
        .unwrap_or(false)
}
