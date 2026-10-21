//! The message composer shared by the channel page and the thread panel
//! (ui.md §4.8-4.9, `MessageComposer.tsx`, `ComposerCompletions.tsx`).
//!
//! It owns the text, mention chips, `@` `#` `:` completion, the emoji
//! picker, the formatting toolbar, attachments and the saved draft. The
//! page that hosts it decides what a submission means (a message, a
//! reply or an edit) and reports back with [`Composer::sent`] or
//! [`Composer::failed`].

pub mod attachments;
mod blocks;
mod channels;
mod clipboard;
mod format;
mod link;
mod live_markdown;
mod mention_picker;
pub(crate) mod mention_rules;
mod mentions;
mod models;
pub(crate) mod poof;

use super::emoji_picker::{self, Candidate, Dismissed, EmojiPicker, Picked};
use super::gif_picker::PickedGif;
use super::media;
use super::media::recorder::{self, Recorder};
use super::message::{self, channel_icon};
use super::message_edit;
use super::overlay;
use crate::community::{AppState, Community};
use attachments::{AttachmentDraft, BackgroundSend, DraftFile, MAX_FILES, NewFile, Queue, Voice};
use buzz_relay_client::fold::MessageRow;
use buzz_relay_client::kit::{KitStatus, KitValue};
use buzz_relay_client::members::can_add_members;
use buzz_relay_client::upload::Source;
use buzz_relay_client::{
    Attachment, ChannelSummary, ChannelType, EventId, Mentions, Partition, PublicChannels,
    RosterStatus,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, ButtonVariant, Dialog, Icon, IconButton,
    IconButtonShape, IconButtonSize, IconName, Spinner, Surface, Toast, ToastTone, Tooltip,
    TypeStyled,
};
use format::Format;
use gpui_kit::component::input::{
    Backspace, Copy, Cut, Enter, Escape, IndentInline, InlineToken, InlineTokenContext,
    InputContent, InputEvent, MoveDown, MoveUp, OutdentInline, Textarea, TextareaState,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use live_markdown::LiveMarkdown;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `maxLength` of the editor.
/// Why a mention could not be inserted: someone is no longer available.
pub(crate) const UNAVAILABLE_MEMBER: &str =
    "A team member is no longer available. Refresh choices before trying again.";
const MAX_CHARS: usize = 16_000;
/// Drafts are saved this long after the last keystroke.
const DRAFT_DELAY: Duration = Duration::from_millis(400);
/// The typing pause before `#` searches open channels.
const PUBLIC_SEARCH_DELAY: Duration = Duration::from_millis(180);
/// Emoji completion rows.
const EMOJI_LIMIT: usize = 100;

actions!(composer, [InsertLink]);

/// Mod+K edits a link in the composer (`EditableInput.tsx`), over the
/// app-wide search it opens elsewhere. GPUI ranks a binding without a
/// context above any with one, so this one has none either: bound after
/// the app's, it is tried first, and with no composer focused nothing
/// handles it and search runs.
struct ComposerKeys;

impl Global for ComposerKeys {}

fn bind_keys(cx: &mut App) {
    if cx.try_global::<ComposerKeys>().is_none() {
        cx.bind_keys([KeyBinding::new("secondary-k", InsertLink, None)]);
        cx.set_global(ComposerKeys);
    }
}

pub enum ComposerEvent {
    Submit(Submission),
    /// Up in an empty composer: edit the viewer's last message.
    EditLast,
    CancelReply,
    /// "Retry edit": republish the edit still in the outbox.
    RetryEdit,
}

#[derive(Clone)]
pub struct Submission {
    pub content: String,
    /// Who the chosen mention chips notify (`p`) and who they only name
    /// (`mention`), first use first (§5d.3).
    pub mentions: Mentions,
    /// The files, which upload in the background after Send
    /// (`sendInBackground`); the host publishes once they are up.
    pub upload: Option<BackgroundSend>,
    /// The message being edited, if any.
    pub editing: Option<Arc<MessageRow>>,
}

impl Submission {
    /// The files to publish: none, or those the background send uploaded.
    /// `None` when that send failed or was cancelled; its text and files
    /// are back in the draft then, so publish nothing.
    pub async fn attachments(&self) -> Option<Vec<Attachment>> {
        match &self.upload {
            Some(upload) => upload.uploaded().await,
            None => Some(Vec::new()),
        }
    }

    /// Reports how publishing went. A failed background send hands its
    /// text and files back with a notice; returns whether it did, in which
    /// case the host shows nothing more.
    pub fn settle(&self, result: &Result<(), String>, cx: &mut App) -> bool {
        match &self.upload {
            Some(upload) => {
                upload.settle(result, cx);
                true
            }
            None => false,
        }
    }
}

/// The thread panel's reply target, shown between the text and the tools.
#[derive(Clone, PartialEq)]
pub enum ReplyContext {
    Parent {
        name: SharedString,
        excerpt: SharedString,
    },
    Missing,
}

impl EventEmitter<ComposerEvent> for Composer {}

/// A file a plugin's capture ends with (`ComposerCaptureProps.accept`);
/// with `voice` (seconds and a 0–255 waveform), a voice note.
pub struct Captured {
    pub name: String,
    pub mime: String,
    /// The bytes, or a file on disk.
    pub source: buzz_relay_client::upload::Source,
    pub size: u64,
    pub voice: Option<(f64, Vec<u8>)>,
}

pub struct Composer {
    community: Entity<Community>,
    channel: String,
    thread_root: Option<EventId>,
    /// The thread's verified root, once known, when the thread opened at
    /// one of its replies: what tools, accessories and completions hear
    /// as its `thread` (`threadRootId={snapshot.root.id}`).
    plugin_root: Option<EventId>,
    input: Entity<TextareaState>,
    /// The Markdown styling of `input`'s text.
    markdown: LiveMarkdown,
    placeholder: SharedString,
    sending: bool,
    /// The host allows sending (a thread needs a loaded reply target).
    can_send: bool,
    /// A new session's composer, which may offer library agents not yet
    /// in the conversation (`inviteAgents`).
    invite_agents: bool,
    /// A conversation view's composer (`sessionConversation`): in a
    /// session it may offer your agents.
    session_conversation: bool,
    /// Eager uploads were paused because the composer is disabled.
    uploads_paused: bool,
    editing: Option<Arc<MessageRow>>,
    /// The text the edit started from.
    edit_initial: Option<String>,
    /// Counts the edits begun here, so late work for one edit (a deletion
    /// asked for by emptying it) leaves a later edit alone.
    edit_generation: u64,
    /// The edit's delivery, as the host reports it.
    edit_status: message_edit::EditStatus,
    /// The draft set aside while editing.
    stash: Option<InputContent>,
    context: Option<ReplyContext>,
    formatting: bool,
    completion: Option<Completion>,
    public: Option<PublicSearch>,
    /// Bumped by "Retry suggestions" to search again.
    public_attempt: u64,
    /// The community people search behind `@` completion.
    directory: Option<DirectorySearch>,
    directory_attempt: u64,
    /// The last finished search's people, shown while the next one runs.
    recent_people: Vec<String>,
    /// A query a complete search found no one for; longer ones can't match.
    refuted: Option<String>,
    /// The @ button's picker, and when a press outside last closed it.
    mention_picker: Option<mention_picker::Picker>,
    mention_picker_closed: Option<std::time::Instant>,
    /// The last open channels found, kept while the next search runs.
    recent_public: Vec<ChannelSummary>,
    /// The query start Escape dismissed, until the text moves on.
    dismissed: Option<usize>,
    /// The text length and caret the completion was computed for.
    completion_key: Option<(usize, usize)>,
    /// Follows the saved teams while `@` completion is used.
    teams_watch: Option<Task<()>>,
    completion_scroll: ScrollHandle,
    picker: Entity<EmojiPicker>,
    picker_open: bool,
    picker_closed: Option<std::time::Instant>,
    /// The draft's files, which outlive this composer.
    files: Entity<AttachmentDraft>,
    /// The community's background sends.
    queue: Entity<Queue>,
    /// "Could not attach file", as a toast.
    attach_error: Option<SharedString>,
    /// First frames of video drafts, by file; `None` while decoding or when
    /// there is none.
    posters: std::cell::RefCell<std::collections::HashMap<String, Option<Arc<RenderImage>>>>,
    /// Removed files still leaving: where they were, and since when.
    exiting: Vec<(usize, DraftFile, Instant)>,
    /// A conversation pane is this composer's drop zone and shows the drop
    /// overlay; otherwise the composer shows it.
    zoned: bool,
    /// Why the last send failed (`error`), under the tools.
    error: Option<SharedString>,
    /// Formats toggled at a collapsed caret, outermost first: their empty
    /// delimiters wait, hidden, around `pending_at` for text.
    pending: Vec<PendingFormat>,
    pending_at: usize,
    /// A formatting edit is being applied, so its changes are not typing.
    applying_format: bool,
    /// The saved draft as this composer last read or wrote it, to notice
    /// another composer's (`revision`); `None` when storage failed.
    revision: Option<Option<String>>,
    draft_issue: Option<DraftIssue>,
    /// The link dialog, with the text it was opened on.
    link: Option<(Entity<link::LinkDialog>, String)>,
    /// "Retry message preparation" was pressed: a ready catalog clears the
    /// error.
    retrying_preparation: bool,
    /// How often the toolbar opened or closed, keying its reveal, and
    /// whether the last time was from the keyboard (instant).
    format_toggles: usize,
    format_keyboard: bool,
    /// The text before the last edit, to refuse one past the limit.
    accepted: Option<(InputContent, std::ops::Range<usize>)>,
    drag_over: bool,
    notice: Option<SharedString>,
    save_draft: Option<Task<()>>,
    /// The send waiting on "Mention people outside this channel?".
    outsiders: Option<Outsiders>,
    /// A voice note being recorded or prepared, in place of the tools.
    capture: Option<Capture>,
    _capture_task: Option<Task<()>>,
    /// The recorder row: Discard while recording, Close after a failure.
    capture_focus: FocusHandle,
    /// The live waveform's level count and when it last grew by one, for
    /// the strip's 5px slide.
    wave_tick: std::cell::Cell<(usize, std::time::Instant)>,
    outsiders_focus: FocusHandle,
    /// The agents the message being sent mentions as recipients, `(pubkey,
    /// chip text)`, kept for the next draft ("Remember mentioned agents").
    followup: Vec<(String, String)>,
    /// Controls on their own row under the tools (Me's assistant and
    /// project); hidden while editing.
    trailing: Option<AnyView>,
    /// Me's chat presentation (#1005): centered at the transcript's
    /// measure, the tools on one row with Send.
    chat: bool,
    /// Me: no remembered-agent follow-up draft after a send.
    no_followup: bool,
    /// Plugin tools in the toolbar (`composer_tools`).
    /// Plugin tools, each with its place among Buzz's own buttons.
    tools: Vec<(super::composer_tools::Slot, super::composer_tools::Built)>,
    /// Plugin accessories above the composer.
    accessories: Vec<super::composer_tools::Built>,
    /// The tool registry's [`super::composer_tools::generation`] the
    /// tools were made at; a plugin adding one rebuilds them.
    tools_generation: u64,
    /// [`super::composer_completions::generation`] when the completion
    /// was last worked out; a plugin's answer arriving works it out again.
    completions_generation: u64,
    /// A new Me conversation's first message carries no files.
    no_attachments: bool,
    /// Model and effort picked for mentioned agents' next run, and the
    /// gauge popover's state.
    run_models: models::RunModels,
    _subscriptions: Vec<Subscription>,
}

/// A format toggled with a collapsed caret, waiting for text (ProseMirror's
/// stored marks). `removes` is a split of a span the caret was in: its
/// text will have the format off.
struct PendingFormat {
    marker: &'static str,
    kind: live_markdown::Kinds,
    removes: bool,
}

/// Why the saved draft could not follow the composer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DraftIssue {
    /// Another composer changed it.
    Conflict,
    /// Storage refused the write.
    Storage,
    /// A message was accepted but its text could not be cleared.
    Accepted,
}

/// Voice note capture (`VoiceCapture.tsx`), or a plugin tool's
/// (`ComposerToolProps.capture`).
enum Capture {
    Recording(Recorder),
    Preparing,
    Failed(SharedString),
    /// A plugin's view in place of the editor until it accepts or the
    /// user discards it.
    Plugin(AnyView),
}

/// Whether a recovered send still owns the saved draft.
fn recovery_owns(saved: Option<&str>, recovered: &str) -> bool {
    saved == Some(recovered)
}

/// `formatVoiceNoteDuration`: whole seconds as `m:ss`.
fn voice_time(seconds: f64) -> String {
    let whole = if seconds.is_finite() {
        seconds.max(0.).floor() as u64
    } else {
        0
    };
    format!("{}:{:02}", whole / 60, whole % 60)
}

/// A send that mentions people outside the channel, held until the user
/// invites them or sends without them (`useNonmemberMentions.tsx`).
struct Outsiders {
    /// Every chosen chip as a recipient.
    submission: Submission,
    missing: Vec<String>,
    can_add: bool,
    busy: bool,
    error: Option<SharedString>,
    /// The text the prompt is about; inviting stops if it changes.
    draft: String,
}

/// What a send with mention chips needs (`MessageComposer.tsx:1069-1101`).
#[derive(Debug, PartialEq)]
enum Admission {
    Send(Mentions),
    /// Some recipients are outside a stream or forum: ask first.
    Ask {
        missing: Vec<String>,
        can_add: bool,
    },
}

/// Recipients must be channel members. A DM (or a session) quietly turns
/// outsiders into references; any other channel asks. With the roster
/// unknown, everyone stays a recipient and the writer's check reports it.
fn admission(channel: Option<&ChannelSummary>, viewer: &str, recipients: Vec<String>) -> Admission {
    let Some(channel) = channel.filter(|c| !c.members.is_empty()) else {
        return Admission::Send(Mentions {
            recipients,
            references: Vec::new(),
            ..Default::default()
        });
    };
    let (members, missing): (Vec<String>, Vec<String>) = recipients
        .iter()
        .cloned()
        .partition(|key| channel.members.contains(key));
    if missing.is_empty() {
        return Admission::Send(Mentions {
            recipients,
            references: Vec::new(),
            ..Default::default()
        });
    }
    if matches!(
        channel.channel_type,
        Some(ChannelType::Dm | ChannelType::Session)
    ) {
        return Admission::Send(Mentions {
            recipients: members,
            references: missing,
            ..Default::default()
        });
    }
    let member = channel.members.iter().any(|key| key == viewer);
    Admission::Ask {
        missing,
        can_add: can_add_members(channel, member),
    }
}

impl Composer {
    pub fn new(
        community: Entity<Community>,
        channel: String,
        thread_root: Option<EventId>,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        bind_keys(cx);
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(placeholder.clone())
                .submit_on_enter(true)
                .auto_grow(1, 9)
        });
        let picker = cx.new(|cx| {
            let mut picker = EmojiPicker::new(community.clone(), window, cx);
            picker.enable_gifs(window, cx);
            picker
        });
        let subscriptions =
            vec![
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                        InputEvent::Change => this.changed(window, cx),
                        _ => {}
                    }
                }),
                cx.subscribe_in(&picker, window, |this, _, Picked(pick), window, cx| {
                    this.picker_open = false;
                    this.insert_text(&pick.text(), window, cx);
                }),
                cx.subscribe_in(
                    &picker,
                    window,
                    |this, _, PickedGif(markdown), window, cx| {
                        this.picker_open = false;
                        this.insert_text(markdown, window, cx);
                    },
                ),
                cx.subscribe_in(&picker, window, |this, _, _: &Dismissed, window, cx| {
                    this.picker_open = false;
                    this.focus(window, cx);
                    cx.notify();
                }),
                cx.observe(&community, |this: &mut Self, community, cx| {
                    if this.retrying_preparation
                        && community.read(cx).snapshot.emoji.status
                            == buzz_relay_client::CatalogStatus::Ready
                    {
                        this.retrying_preparation = false;
                        this.error = None;
                    }
                    cx.notify()
                }),
            ];
        let key = draft_key(&channel, thread_root);
        let files = attachments::draft(&community, &key, cx);
        attachments::view(&files, cx);
        let queue = attachments::queue(&community, cx);
        let host = cx.entity_id();
        queue.update(cx, |queue, cx| queue.add_host(host, cx));
        let mut subscriptions = subscriptions;
        subscriptions.push(cx.observe_in(&files, window, |this, _, window, cx| {
            this.take_recovered(window, cx);
            cx.notify();
        }));
        subscriptions.push(cx.observe(&queue, |_, _, cx| cx.notify()));
        // Mentioned agents may become the viewer's, or change type.
        if let Some(agents) = crate::agents2::Agents2::global(cx) {
            subscriptions.push(cx.observe(&agents, |_, _, cx| cx.notify()));
        }
        cx.on_release(move |this: &mut Self, cx| {
            this.write_draft(cx);
            let key = this.draft_key();
            attachments::leave(&this.community, &key, &this.files, cx);
            this.queue
                .update(cx, |queue, cx| queue.remove_host(host, cx));
        })
        .detach();
        let markdown = LiveMarkdown::new(&input, cx);
        // Tools and accessories are made when the composer first shows, as
        // buzz-app mounts them with it, so they open with the thread's
        // root once its host knows it.
        let (tools, accessories) = (Vec::new(), Vec::new());
        let mut composer = Self {
            community,
            channel,
            thread_root,
            plugin_root: None,
            input,
            markdown,
            placeholder,
            sending: false,
            can_send: true,
            invite_agents: false,
            session_conversation: false,
            uploads_paused: false,
            editing: None,
            edit_generation: 0,
            edit_initial: None,
            edit_status: message_edit::EditStatus::default(),
            stash: None,
            context: None,
            formatting: false,
            completion: None,
            public: None,
            public_attempt: 0,
            directory: None,
            directory_attempt: 0,
            recent_people: Vec::new(),
            refuted: None,
            mention_picker: None,
            mention_picker_closed: None,
            recent_public: Vec::new(),
            dismissed: None,
            completion_key: None,
            teams_watch: None,
            completion_scroll: ScrollHandle::new(),
            picker,
            picker_open: false,
            picker_closed: None,
            files,
            queue,
            attach_error: None,
            exiting: Vec::new(),
            zoned: false,
            posters: Default::default(),
            error: None,
            pending: Vec::new(),
            pending_at: 0,
            applying_format: false,
            accepted: None,
            format_toggles: 0,
            format_keyboard: false,
            revision: None,
            draft_issue: None,
            retrying_preparation: false,
            link: None,
            drag_over: false,
            notice: None,
            save_draft: None,
            outsiders: None,
            outsiders_focus: cx.focus_handle(),
            capture: None,
            _capture_task: None,
            capture_focus: cx.focus_handle(),
            wave_tick: std::cell::Cell::new((0, std::time::Instant::now())),
            followup: Vec::new(),
            trailing: None,
            chat: false,
            no_followup: false,
            tools,
            accessories,
            tools_generation: UNBUILT,
            completions_generation: super::composer_completions::generation(),
            no_attachments: false,
            run_models: models::RunModels::default(),
            _subscriptions: subscriptions,
        };
        composer.load_draft(window, cx);
        composer.take_missed_recovery(window, cx);
        composer
    }

    /// Offers no attachments (a new conversation's first message).
    pub fn without_attachments(&mut self) {
        self.no_attachments = true;
    }

    /// May offer library agents not yet in the conversation.
    pub fn invite_agents(&mut self) {
        self.invite_agents = true;
    }

    /// Whether it may offer library agents, as a plugin's composer element
    /// says on each draw.
    pub fn set_invite_agents(&mut self, invite: bool) {
        self.invite_agents = invite;
    }

    /// A conversation view's own composer, which in a session may offer
    /// your agents (`sessionConversation`); embedded and media composers
    /// are not.
    pub fn conversation_view(&mut self) {
        self.session_conversation = true;
    }

    /// Makes or remakes the tools and accessories, as buzz-app mounts them
    /// with the composer: when it shows, or when its host keeps it mounted
    /// out of sight (an unselected tab, closed comments).
    pub fn mount_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = super::composer_tools::generation(cx);
        if generation == self.tools_generation {
            return;
        }
        self.tools_generation = generation;
        let context = super::composer_tools::ToolContext {
            community: self.community.clone(),
            channel: self.channel.clone(),
            thread: self.plugin_thread(),
            composer: cx.weak_entity(),
        };
        self.tools = super::composer_tools::build(context.clone(), &self.tools, window, cx);
        self.accessories =
            super::composer_tools::build_accessories(context, &self.accessories, window, cx);
    }

    /// The thread plugins hear this composer replies in.
    fn plugin_thread(&self) -> Option<String> {
        self.plugin_root
            .or(self.thread_root)
            .map(|root| root.to_hex())
    }

    /// The thread's verified root: tools and accessories open again with
    /// it as their `thread`, as buzz-app mounts its composer keyed by it.
    pub fn set_thread_root(&mut self, root: EventId, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_root.or(self.thread_root) == Some(root) {
            return;
        }
        self.plugin_root = Some(root);
        if self.tools_generation == UNBUILT {
            return;
        }
        let context = super::composer_tools::ToolContext {
            community: self.community.clone(),
            channel: self.channel.clone(),
            thread: self.plugin_thread(),
            composer: cx.weak_entity(),
        };
        self.tools = super::composer_tools::build(context.clone(), &[], window, cx);
        self.accessories = super::composer_tools::build_accessories(context, &[], window, cx);
        cx.notify();
    }

    /// Not one after all, as the Inbox's fallback thread reader is not.
    pub fn not_conversation_view(&mut self) {
        self.session_conversation = false;
    }

    /// The community this composer writes in.
    pub fn community(&self) -> &Entity<Community> {
        &self.community
    }

    /// Your library agents may be offered here (`inviteAgents`): in a new
    /// session or a session conversation's own view, never while editing.
    pub fn invites_agents(&self, cx: &App) -> bool {
        self.editing.is_none()
            && (self.invite_agents
                || (self.session_conversation
                    && self
                        .summary(cx)
                        .is_some_and(|channel| channel.channel_type == Some(ChannelType::Session))))
    }

    /// Nothing can be typed or sent now (`ComposerToolProps.disabled`).
    pub fn disabled(&self, cx: &App) -> bool {
        !self.can_send
            || self.input_disabled(cx)
            || self.summary(cx).is_some_and(|channel| channel.read_only)
    }

    /// Me's new-chat composer: `trailing` controls below its tools, at
    /// the full width, and no remembered-agent follow-up.
    pub fn set_trailing(&mut self, trailing: AnyView, cx: &mut Context<Self>) {
        self.no_followup = true;
        self.trailing = Some(trailing);
        cx.notify();
    }

    /// Me's chat composer: centered, with `trailing` controls below its
    /// tools, and no remembered-agent follow-up (`chatPresentation`).
    pub fn set_chat(&mut self, trailing: Option<AnyView>, cx: &mut Context<Self>) {
        self.chat = true;
        self.no_followup = true;
        self.trailing = trailing;
        cx.notify();
    }

    /// The text field, for focus.
    pub fn input(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// A send is in flight, so the draft cannot change.
    pub fn sending(&self) -> bool {
        self.sending
    }

    /// The error the composer shows now, if any.
    pub fn error_text(&self) -> Option<String> {
        self.error.as_ref().map(ToString::to_string)
    }

    #[allow(dead_code)]
    pub fn editing(&self) -> Option<&Arc<MessageRow>> {
        self.editing.as_ref()
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if placeholder == self.placeholder {
            return;
        }
        self.placeholder = placeholder.clone();
        if self.editing.is_none() {
            self.input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
        }
    }

    pub fn set_context(&mut self, context: Option<ReplyContext>, cx: &mut Context<Self>) {
        if self.context != context {
            self.context = context;
            cx.notify();
        }
    }

    /// A disabled composer (the host's, or a read-only or archived
    /// channel) pauses its draft's eager uploads; enabling it resumes them
    /// without a retry.
    fn sync_uploads(&mut self, cx: &mut Context<Self>) {
        let disabled = !self.can_send
            || self
                .summary(cx)
                .is_some_and(|channel| channel.archived || channel.read_only);
        if disabled != self.uploads_paused {
            self.uploads_paused = disabled;
            self.files.update(cx, |files, cx| {
                if disabled {
                    files.cancel(cx)
                } else {
                    files.resume(cx)
                }
            });
        }
    }

    pub fn set_can_send(&mut self, can_send: bool, cx: &mut Context<Self>) {
        if self.can_send != can_send {
            self.can_send = can_send;
            cx.notify();
        }
    }

    /// The edit in progress, if any, as `start_edit` numbered it.
    pub fn edit_session(&self) -> Option<(EventId, u64)> {
        self.editing
            .as_ref()
            .map(|row| (row.id, self.edit_generation))
    }

    /// The text box has keyboard focus.
    pub fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// The text the current edit started from.
    pub fn edit_initial(&self) -> Option<&str> {
        self.edit_initial.as_deref()
    }

    /// Shows the edit's delivery: locked while sent, busy while sending,
    /// and "Retry edit" when it failed.
    pub fn set_edit_status(&mut self, status: message_edit::EditStatus, cx: &mut Context<Self>) {
        if self.edit_status != status {
            self.edit_status = status;
            cx.notify();
        }
    }

    /// A message under the text, such as why an edit cannot be saved.
    pub fn set_notice(&mut self, notice: Option<SharedString>, cx: &mut Context<Self>) {
        self.notice = notice;
        self.sending = false;
        cx.notify();
    }

    /// Puts `row`'s text in the composer to edit it (§4.8 edit mode). The
    /// draft waits until the edit ends. Another edit must end first.
    pub fn start_edit(
        &mut self,
        row: Arc<MessageRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| editing.id != row.id)
        {
            self.notice = Some(message_edit::BUSY.into());
            self.input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
            return;
        }
        if self.editing.is_none() {
            self.write_draft(cx);
            self.stash = Some(self.input.read(cx).content());
            self.edit_generation += 1;
        }
        let text = {
            let community = self.community.read(cx);
            let directory = super::markdown::Directory::of(community, None, cx);
            message_edit::edit_text(&row, &directory.profiles, &directory.names)
        };
        let content = edit_content(&text);
        let end = content.text().len();
        self.edit_initial = Some(content.text().to_string());
        self.edit_status = message_edit::EditStatus::default();
        self.notice = None;
        self.editing = Some(row);
        self.completion = None;
        self.input.update(cx, |state, cx| {
            state.set_value(content, window, cx);
            state.set_selected_range(end..end, cx);
            state.set_placeholder("Edit message", window, cx);
            state.focus(window, cx);
        });
        self.restyle(cx);
        cx.notify();
    }

    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit_status.busy || self.editing.take().is_none() {
            return;
        }
        self.edit_initial = None;
        self.edit_status = message_edit::EditStatus::default();
        self.notice = None;
        self.sending = false;
        let stash = self.stash.take().unwrap_or_else(|| InputContent::new(""));
        let placeholder = self.placeholder.clone();
        self.input.update(cx, |state, cx| {
            let end = stash.text().len();
            state.set_value(stash, window, cx);
            state.set_selected_range(end..end, cx);
            state.set_placeholder(placeholder, window, cx);
        });
        self.restyle(cx);
        self.completion = None;
        cx.notify();
    }

    /// The host delivered the submission: clear it. A background send
    /// cleared the composer at Send, so this does nothing then.
    pub fn sent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sending {
            return;
        }
        self.sending = false;
        self.notice = None;
        if self.editing.is_some() {
            self.cancel_edit(window, cx);
            return;
        }
        self.clear_after_send(window, cx);
    }

    /// Empties the composer after a send, leaving the follow-up draft of
    /// remembered agents.
    fn clear_after_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.completion = None;
        self.run_models.picks.clear();
        let followup = std::mem::take(&mut self.followup);
        let next = if !self.no_followup && crate::ui::settings::remember_mentioned_agents(cx) {
            let (text, chips) = crate::ui::settings::followup_draft(&followup);
            let mut content = InputContent::new(text);
            for (range, id, chip) in chips {
                let token = InlineToken::new(id, chip);
                if let Ok(next) = content.clone().with_token(range, token) {
                    content = next;
                }
            }
            content
        } else {
            InputContent::new(String::new())
        };
        let end = next.text().len();
        self.input.update(cx, |state, cx| {
            state.set_value(next, window, cx);
            state.set_selected_range(end..end, cx);
        });
        self.restyle(cx);
        self.save_draft = None;
        if !self.persist(true, cx) {
            // The message went; the sent text is still saved (`accepted`).
            self.draft_issue = Some(DraftIssue::Accepted);
        }
        cx.notify();
    }

    /// The host could not deliver it; the text stays.
    pub fn failed(&mut self, cx: &mut Context<Self>) {
        self.sending = false;
        cx.notify();
    }

    /// The channel as discovery last described it.
    fn summary<'a>(&self, cx: &'a App) -> Option<&'a ChannelSummary> {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
    }

    /// The text is not editable while a send is admitted or an edit saves,
    /// in an archived channel (its draft stays), and in one known only from
    /// the cache (`editingDisabled`).
    fn input_disabled(&self, cx: &App) -> bool {
        self.sending
            || self.edit_status.locked
            || self.outsiders.as_ref().is_some_and(|prompt| prompt.busy)
            || self
                .summary(cx)
                .is_some_and(|channel| channel.archived || channel.cached)
    }

    fn has_text(&self, cx: &App) -> bool {
        !self.input.read(cx).value().trim().is_empty()
    }

    fn has_files(&self, cx: &App) -> bool {
        !self.files.read(cx).items.is_empty()
    }

    /// A send of this conversation is still uploading.
    fn send_pending(&self, cx: &App) -> bool {
        self.queue.read(cx).pending(&self.draft_key())
    }

    fn can_submit(&self, cx: &App) -> bool {
        let files = self.files.read(cx);
        !self.input_disabled(cx)
            && !self.sending
            && self.can_send
            && (self.editing.is_some() || (!files.blocked() && !self.send_pending(cx)))
            && self.input.read(cx).value().chars().count() <= MAX_CHARS
            && !self.edit_status.locked
            && (self.editing.is_some()
                || !matches!(self.draft_issue, Some(DraftIssue::Conflict | DraftIssue::Accepted)))
            // An empty edit asks to delete the message instead.
            && (self.has_text(cx) || self.editing.is_some() || !files.items.is_empty())
    }

    /// Hands a submission to the host. With files, Send clears the composer
    /// at once and they upload in the background before the host publishes.
    fn dispatch(
        &mut self,
        mut submission: Submission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        // The whole captured pick set returns with a recovered draft; only
        // agents still notified by this message are asked.
        let picks = submission.mentions.picks.clone();
        submission.mentions.picks = models::sent_picks(&picks, &submission.mentions.recipients);
        let has_files = !self.files.read(cx).items.is_empty();
        if submission.editing.is_none() && has_files {
            // A file that failed meanwhile (say, during the outside-channel
            // prompt) waits for Retry or removal.
            if self.files.read(cx).blocked() {
                cx.notify();
                return;
            }
            let draft = self.draft_json(cx);
            self.clear_after_send(window, cx);
            let followup = self.draft_json(cx);
            let (owner, files) = (self.draft_key(), self.files.clone());
            // Send takes the files with their running upload; clearing the
            // composer does not stop it.
            submission.upload = self.queue.update(cx, |queue, cx| {
                queue.start(owner, &files, draft, followup, picks, cx)
            });
            self.sending = false;
        } else {
            self.sending = true;
        }
        cx.emit(ComposerEvent::Submit(submission));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        // Another composer saved this draft meanwhile: settle that first.
        if self.editing.is_none()
            && let (Ok(current), Some(revision)) = (self.stored_draft(cx), &self.revision)
            && current != *revision
        {
            let ours = self.draft_json(cx);
            if current.as_deref() != Some(ours.as_str()) {
                self.draft_issue = Some(DraftIssue::Conflict);
                cx.notify();
                return;
            }
        }
        let state = self.input.read(cx);
        let content = state.value().trim().to_string();
        let _ = window;
        let chosen = self.recipients(cx);
        if self.editing.is_none()
            && chosen
                .iter()
                .any(|(key, _)| super::archives::hides(&self.community, key, cx))
        {
            self.error = Some("A selected recipient is archived. Remove it before sending.".into());
            cx.notify();
            return;
        }
        let mentioned: Vec<String> = chosen.iter().map(|(key, _)| key.clone()).collect();
        // Agents among the recipients, for the next draft.
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        self.followup = chosen
            .into_iter()
            .filter(|(key, _)| profiles.get(key).is_some_and(|profile| profile.is_agent))
            .map(|(key, name)| (key, format!("@{name}")))
            .collect();
        self.sending = true;
        self.completion = None;
        // An edit, or a new conversation's first message, asks for no run.
        let picks = if self.editing.is_none() && !self.no_attachments {
            self.run_models.picks.clone()
        } else {
            Default::default()
        };
        let mut submission = Submission {
            content,
            mentions: Mentions {
                recipients: mentioned,
                references: Vec::new(),
                picks,
            },
            upload: None,
            editing: self.editing.clone(),
        };
        if self.editing.is_none() && !submission.mentions.recipients.is_empty() {
            let community = self.community.read(cx);
            let channel = community
                .snapshot
                .channels
                .iter()
                .find(|c| c.id == self.channel);
            let viewer = community.session.viewer();
            match admission(channel, viewer, submission.mentions.recipients.clone()) {
                Admission::Send(mentions) => {
                    submission.mentions = Mentions {
                        picks: std::mem::take(&mut submission.mentions.picks),
                        ..mentions
                    }
                }
                Admission::Ask { missing, can_add } => {
                    let draft = self.input.read(cx).value().to_string();
                    self.outsiders = Some(Outsiders {
                        submission,
                        missing,
                        can_add,
                        busy: false,
                        error: None,
                        draft,
                    });
                    self.outsiders_focus.focus(window, cx);
                    cx.notify();
                    return;
                }
            }
        }
        self.dispatch(submission, window, cx);
    }

    /// Pubkeys and names of the mention chips, first use first: the
    /// explicit recipients (`MentionDraft.recipients`).
    fn recipients(&self, cx: &App) -> Vec<(String, SharedString)> {
        let mut recipients: Vec<(String, SharedString)> = Vec::new();
        for span in self.input.read(cx).tokens() {
            let token = span.token();
            let id = token.id().to_string();
            if let Some(name) = token.text().strip_prefix('@')
                && !recipients.iter().any(|(key, _)| *key == id)
            {
                recipients.push((id, SharedString::from(name.to_string())));
            }
        }
        recipients
    }

    /// Turns every chip for `pubkey` back into plain `@Name` text, so the
    /// message names them without notifying them (`removeRecipient`).
    fn remove_recipient(&mut self, pubkey: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        self.input.update(cx, |state, cx| {
            let content = state.content();
            let selection = state.selected_range();
            let mut next = InputContent::new(content.text().clone());
            for span in content.tokens() {
                let token = span.token();
                if token.id() == pubkey && token.text().starts_with('@') {
                    continue;
                }
                if let Ok(with) = next.clone().with_token(span.range().clone(), token.clone()) {
                    next = with;
                }
            }
            state.set_value(next, window, cx);
            state.set_selected_range(selection, cx);
            state.focus(window, cx);
        });
        self.restyle(cx);
        // The draft keeps chips, so it must forget this one too.
        self.schedule_draft(cx);
        cx.notify();
    }

    // ----- mentions outside the channel -----

    /// Close or Escape: nothing is sent and the draft stays.
    fn close_outsiders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.outsiders.as_ref().is_none_or(|prompt| prompt.busy) {
            return;
        }
        self.outsiders = None;
        self.sending = false;
        self.focus(window, cx);
        cx.notify();
    }

    /// "Do nothing" / "Send anyway": outsiders become display-only
    /// references and are not added.
    fn send_without_outsiders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.outsiders.as_ref().is_none_or(|prompt| prompt.busy) {
            return;
        }
        let Some(prompt) = self.outsiders.take() else {
            return;
        };
        let mut submission = prompt.submission;
        submission
            .mentions
            .recipients
            .retain(|key| !prompt.missing.contains(key));
        submission.mentions.references = prompt.missing;
        self.dispatch(submission, window, cx);
    }

    /// "Invite": adds each outsider (kind 9000) and waits for the roster,
    /// then sends with everyone notified. A failure keeps the dialog open.
    fn invite_outsiders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.outsiders.as_mut().filter(|p| p.can_add && !p.busy) else {
            return;
        };
        prompt.busy = true;
        prompt.error = None;
        let missing = prompt.missing.clone();
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut failure = None;
            for key in &missing {
                let changed = this
                    .update(cx, |this, cx| {
                        this.outsiders.as_ref().is_none_or(|prompt| {
                            this.input.read(cx).value().as_ref() != prompt.draft.as_str()
                        })
                    })
                    .unwrap_or(true);
                if changed {
                    failure = Some("The draft changed. Close this dialog and send again.".into());
                    break;
                }
                if let Err(error) = session.add_member(&channel, key).await {
                    failure = Some(error);
                    break;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                match failure {
                    Some(error) => {
                        if let Some(prompt) = &mut this.outsiders {
                            prompt.busy = false;
                            prompt.error = Some(
                                format!(
                                    "{error} Your draft is kept. Some people may already have \
                                     been added."
                                )
                                .into(),
                            );
                        }
                    }
                    None => {
                        if let Some(prompt) = this.outsiders.take() {
                            this.dispatch(prompt.submission, window, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.applying_format {
            self.settle_pending(window, cx);
        }
        // The editor holds at most `MAX_CHARS` (`maxLength`): an edit past
        // it is refused.
        if self.input.read(cx).value().chars().count() > MAX_CHARS {
            if let Some((content, selection)) = self.accepted.clone() {
                self.input.update(cx, |state, cx| {
                    state.set_value(content, window, cx);
                    state.set_selected_range(selection, cx);
                });
            }
            self.error = Some("Message is too long to insert text".into());
            self.restyle(cx);
            cx.notify();
            return;
        }
        {
            let state = self.input.read(cx);
            self.accepted = Some((state.content(), state.selected_range()));
        }
        self.restyle(cx);
        if self.accept_terminator(window, cx) {
            return;
        }
        self.schedule_draft(cx);
        self.refresh_completion(cx);
        cx.notify();
    }

    /// Styles the Markdown in the text. `set_value` reports no change, so
    /// the places that call it restyle here too.
    fn restyle(&mut self, cx: &mut Context<Self>) {
        let pending = self.pending_ranges();
        self.markdown.restyle(&self.input, &pending, cx);
    }

    // ----- pending formats -----

    /// The pending delimiters around the caret: openings outermost first,
    /// then closings innermost first.
    fn pending_text(&self) -> (String, String) {
        let open: String = self.pending.iter().map(|p| p.marker).collect();
        let close: String = self.pending.iter().rev().map(|p| p.marker).collect();
        (open, close)
    }

    /// Each pending delimiter's range, to keep hidden.
    fn pending_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let mut ranges = Vec::new();
        let (open, _) = self.pending_text();
        let mut at = self.pending_at.saturating_sub(open.len());
        for pending in &self.pending {
            ranges.push(at..at + pending.marker.len());
            at += pending.marker.len();
        }
        for pending in self.pending.iter().rev() {
            ranges.push(at..at + pending.marker.len());
            at += pending.marker.len();
        }
        ranges
    }

    /// Whether the pending delimiters still sit, empty, around `caret`.
    fn pending_holds(&self, text: &str, caret: usize) -> bool {
        let (open, close) = self.pending_text();
        caret >= open.len()
            && text.get(caret - open.len()..caret) == Some(open.as_str())
            && text.get(caret..caret + close.len()) == Some(close.as_str())
    }

    /// After an edit: text typed at the caret fills the pending formats,
    /// which then style it; a deletion that moved the caret off them drops
    /// them, as ProseMirror drops stored marks.
    fn settle_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_empty() {
            return;
        }
        let (text, caret) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range().end)
        };
        if self.pending_holds(&text, caret) {
            self.pending_at = caret;
            return;
        }
        let (open, close) = self.pending_text();
        let both = format!("{open}{close}");
        if text.get(caret..caret + both.len()) == Some(both.as_str()) {
            let range = caret..caret + both.len();
            self.pending.clear();
            self.applying_format = true;
            self.input.update(cx, |state, cx| {
                state.set_selected_range(range, cx);
                state.replace("", window, cx);
            });
            self.applying_format = false;
        }
        self.pending.clear();
    }

    /// The caret left the pending formats without typing: they go away.
    fn leave_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        if self.pending.is_empty() || (selection.is_empty() && selection.end == self.pending_at) {
            return;
        }
        let at = self.pending_at;
        let holds = self.pending_holds(&text, at);
        let (open, close) = self.pending_text();
        self.pending.clear();
        if holds {
            let range = at - open.len()..at + close.len();
            let removed = range.len();
            let map = |offset: usize| {
                if offset >= range.end {
                    offset - removed
                } else {
                    offset.min(range.start)
                }
            };
            let next = map(selection.start)..map(selection.end);
            self.applying_format = true;
            self.input.update(cx, |state, cx| {
                state.set_selected_range(range, cx);
                state.replace("", window, cx);
                state.set_selected_range(next, cx);
            });
            self.applying_format = false;
        }
        self.restyle(cx);
    }

    /// The formats pressed in the toolbar: those at the caret, with the
    /// pending ones on and the ones a pending split turns off.
    fn active_formats(&self, cx: &App) -> live_markdown::Active {
        let state = self.input.read(cx);
        let (add, remove) = self.pending.iter().fold((0, 0), |(add, remove), p| {
            if p.removes {
                (add, remove | p.kind)
            } else {
                (add | p.kind, remove)
            }
        });
        let mut active = self
            .markdown
            .active(state.selected_range(), state.value().as_ref(), add);
        if remove & live_markdown::STRONG != 0 {
            active.bold = false;
        }
        if remove & live_markdown::EMPHASIS != 0 {
            active.italic = false;
        }
        if remove & live_markdown::STRIKE != 0 {
            active.strike = false;
        }
        if remove & live_markdown::CODE != 0 {
            active.code = false;
        }
        if remove & live_markdown::SPOILER != 0 {
            active.spoiler = false;
        }
        active
    }

    fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.replace(text, window, cx);
        });
        cx.notify();
    }

    // ----- drafts -----

    fn draft_key(&self) -> String {
        draft_key(&self.channel, self.thread_root)
    }

    /// The draft as saved, `{text, tokens}` JSON.
    fn draft_json(&self, cx: &App) -> String {
        let content = self.input.read(cx).content();
        serde_json::to_string(&Draft::from(&content)).expect("drafts serialize")
    }

    /// A failed send handed its text back: show it, unless the composer
    /// holds something else by now (its saved draft was checked already).
    fn take_recovered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recovered) = self.files.update(cx, |files, _| files.recovered.take()) else {
            return;
        };
        self.error = recovered.error.clone();
        self.revision = Some(Some(recovered.draft.clone()));
        self.draft_issue = None;
        // The picks belong to this revision: they come back with it.
        self.run_models.picks = recovered.picks.clone();
        if self.editing.is_some() {
            self.stash = serde_json::from_str::<Draft>(&recovered.draft)
                .ok()
                .map(|draft| draft.content());
        } else if let Ok(draft) = serde_json::from_str::<Draft>(&recovered.draft) {
            let content = draft.content();
            let end = content.text().len();
            self.input.update(cx, |state, cx| {
                state.set_value(content, window, cx);
                state.set_selected_range(end..end, cx);
            });
            self.restyle(cx);
        }
        cx.notify();
    }

    /// A recovery that landed while no composer showed this draft: taken
    /// only while its saved text still owns the draft, so an old error or
    /// pick never attaches to a replacement written meanwhile.
    fn take_missed_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self
            .files
            .read(cx)
            .recovered
            .as_ref()
            .map(|r| r.draft.clone())
        else {
            return;
        };
        if recovery_owns(self.stored_draft(cx).ok().flatten().as_deref(), &draft) {
            self.take_recovered(window, cx);
        } else {
            self.files.update(cx, |files, _| files.recovered = None);
        }
    }

    fn partition(&self, cx: &App) -> Partition {
        self.community.read(cx).partition()
    }

    /// The saved draft as stored now; `Err` when storage cannot be read.
    fn stored_draft(&self, cx: &App) -> Result<Option<String>, ()> {
        cx.global::<AppState>()
            .store
            .view_state(&self.partition(cx), &self.draft_key())
            .map_err(|_| ())
    }

    fn load_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.stored_draft(cx);
        self.revision = stored.clone().ok();
        if stored.is_err() {
            self.draft_issue = Some(DraftIssue::Storage);
        }
        let saved = stored
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Draft>(&json).ok());
        let content = saved.map_or_else(|| InputContent::new(""), |draft| draft.content());
        {
            let end = content.text().len();
            self.input.update(cx, |state, cx| {
                state.set_value(content, window, cx);
                state.set_selected_range(end..end, cx);
            });
            self.restyle(cx);
        }
    }

    fn schedule_draft(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        self.save_draft = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAFT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.save_draft = None;
                this.write_draft(cx);
            });
        }));
    }

    /// Saves the draft unless another composer changed the saved one since
    /// this one read it (`replaceView`); that is a conflict for the user to
    /// settle. `force` keeps this composer's draft over the saved one.
    fn write_draft(&mut self, cx: &mut App) {
        self.persist(false, cx);
    }

    fn persist(&mut self, force: bool, cx: &mut App) -> bool {
        if self.editing.is_some() || (self.draft_issue == Some(DraftIssue::Conflict) && !force) {
            return false;
        }
        let content = self.input.read(cx).content();
        let value = (!content.text().trim().is_empty())
            .then(|| serde_json::to_string(&Draft::from(&content)).expect("drafts serialize"));
        let current = self.stored_draft(cx);
        if !force
            && let (Ok(current), Some(revision)) = (&current, &self.revision)
            && current != revision
            && *current != value
        {
            self.draft_issue = Some(DraftIssue::Conflict);
            return false;
        }
        let saved = cx.global::<AppState>().store.set_view_state(
            &self.partition(cx),
            &self.draft_key(),
            value.as_deref(),
        );
        match saved {
            Ok(()) => {
                self.revision = Some(value);
                self.draft_issue = None;
                true
            }
            Err(error) => {
                tracing::warn!(%error, "could not save the draft");
                if self.draft_issue != Some(DraftIssue::Accepted) {
                    self.draft_issue = Some(DraftIssue::Storage);
                }
                false
            }
        }
    }

    /// "Load saved draft": the other composer's draft wins.
    fn load_saved_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_issue = None;
        self.load_draft(window, cx);
        cx.notify();
    }

    // ----- completion -----

    /// Recomputes the `@` `#` `:` popup for the text before the caret.
    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let state = self.input.read(cx);
        let text = state.value().to_string();
        let selection = state.selected_range();
        self.completion_key = Some((text.len(), selection.end));
        let query = (selection.is_empty())
            .then(|| {
                find_query(
                    &text,
                    selection.end,
                    &super::composer_completions::triggers(),
                )
            })
            .flatten()
            // Buzz's own `@`, `#` and `:` are their plugins': off, they
            // offer nothing.
            .filter(|query| match query.trigger {
                Trigger::Mention => crate::plugins::enabled("buzz.mentions"),
                Trigger::Channel => crate::plugins::enabled("buzz.channels"),
                Trigger::Emoji => crate::plugins::enabled("buzz.emoji"),
                _ => true,
            })
            .filter(|query| {
                // A chip's own `@Name` is not a search.
                !state
                    .tokens()
                    .iter()
                    .any(|span| span.range().contains(&query.start))
            });
        let Some(query) = query else {
            self.completion = None;
            self.dismissed = None;
            return;
        };
        if self.dismissed == Some(query.start) {
            self.completion = None;
            return;
        }
        self.dismissed = None;
        let mut space = None;
        let (items, status, retry) = match query.trigger {
            Trigger::Channel => {
                self.search_public(&query.text, cx);
                self.channel_suggestions(&query, cx)
            }
            Trigger::Mention => {
                super::archives::ensure(&self.community, false, cx);
                self.watch_teams(cx);
                self.search_people(&query.text, cx);
                let Some(found) = self.mention_suggestions(&query, cx) else {
                    self.completion = None;
                    return;
                };
                space = found.space;
                // Rows already shown for this query stay, disabled with a
                // reason when they can no longer be picked (`use-mention-choices`).
                let items = match self.completion.as_ref().filter(|old| {
                    old.query.trigger == Trigger::Mention
                        && old.query.start == query.start
                        && old.query.text == query.text
                }) {
                    Some(old) => keep_shown(&old.items, found.items, |key| {
                        if super::archives::hides(&self.community, key, cx) {
                            "Archived"
                        } else {
                            "No longer available. Change your search to refresh choices."
                        }
                    }),
                    None => found.items,
                };
                (items, found.status, found.retry)
            }
            Trigger::Plugin(key) => {
                let thread = self.plugin_thread();
                let answer = super::composer_completions::get(key).and_then(|completion| {
                    let chars = |byte: usize| text[..byte.min(text.len())].chars().count();
                    let community = self.community.read(cx).origin.websocket().to_string();
                    let place = super::composer_completions::Place {
                        community: &community,
                        channel: &self.channel,
                        thread: thread.as_deref(),
                        invite_agents: self.invites_agents(cx),
                        draft: &text,
                        start: chars(query.start),
                        end: chars(query.end),
                    };
                    (completion.query)(&query.text, place, cx)
                });
                let profiles = self.community.read(cx).snapshot.profiles.clone();
                match answer {
                    // Nothing shows until the plugin answers or publishes,
                    // as in buzz-app.
                    None => (Vec::new(), None, None),
                    Some(answer) => (
                        answer
                            .items
                            .into_iter()
                            .take(50)
                            .map(|item| {
                                if item.mentions.is_empty() {
                                    let leading = match (&item.avatar, &item.image) {
                                        (Some(key), _) => {
                                            let profile = profiles.get(key);
                                            Some((
                                                profile.map_or_else(
                                                    || item.label.clone(),
                                                    |profile| profile.name.clone().into(),
                                                ),
                                                profile.and_then(|profile| profile.picture.clone()),
                                            ))
                                        }
                                        (None, Some(image)) => {
                                            Some((item.label.clone(), Some(image.clone())))
                                        }
                                        (None, None) => None,
                                    };
                                    return Suggestion::Text {
                                        label: item.label,
                                        detail: item.detail,
                                        text: item.text,
                                        disabled: item.disabled,
                                        preview: item.preview,
                                        exact: item.exact,
                                        leading,
                                    };
                                }
                                // Picked as a team is: a chip for each.
                                let recipients = item
                                    .mentions
                                    .iter()
                                    .map(|key| {
                                        let name = item
                                            .names
                                            .get(key)
                                            .cloned()
                                            .or_else(|| {
                                                profiles
                                                    .get(key)
                                                    .map(|profile| profile.name.clone())
                                            })
                                            .unwrap_or_else(|| key.chars().take(10).collect());
                                        (key.clone(), SharedString::from(name), None)
                                    })
                                    .collect();
                                Suggestion::Team {
                                    name: item.label,
                                    recipients,
                                    detail: item
                                        .disabled
                                        .clone()
                                        .or(item.detail)
                                        .or(item.preview)
                                        .unwrap_or_default(),
                                    disabled: item.disabled.is_some(),
                                    exact: item.exact,
                                }
                            })
                            .collect(),
                        answer.status,
                        answer.retry.then_some(channels::Retry::Plugin(key)),
                    ),
                }
            }
            Trigger::Emoji => {
                // Unicode results still show while the community catalog
                // is unavailable (`EmojiCompletion`).
                let failed = matches!(
                    self.community.read(cx).snapshot.emoji.status,
                    buzz_relay_client::CatalogStatus::Error(_)
                );
                (
                    self.suggestions(&query, cx),
                    failed.then(|| "Community emoji unavailable; Unicode results shown.".into()),
                    failed.then_some(channels::Retry::Emoji),
                )
            }
        };
        let count = items.len() + usize::from(retry.is_some());
        if count == 0 && status.is_none() {
            self.completion = None;
            return;
        }
        let selected = match &self.completion {
            Some(old) if old.query.start == query.start && old.query.trigger == query.trigger => {
                old.selected.min(count.saturating_sub(1))
            }
            _ => 0,
        };
        self.completion = Some(Completion {
            query,
            items,
            selected,
            status,
            retry,
            space,
        });
    }

    /// Starts the community people search for `@query` after a 200ms
    /// typing pause (`useMentionDirectory`). Sessions never search, and a
    /// query that extends one a complete search refuted is not searched.
    fn search_people(&mut self, query: &str, cx: &mut Context<Self>) {
        let needle = query.trim().to_string();
        let community = self.community.read(cx);
        let channel = community
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel);
        if needle.is_empty()
            || !mentions::searches_directory(channel)
            || community.snapshot.roster != RosterStatus::Ready
        {
            self.directory = None;
            return;
        }
        if self.directory.as_ref().is_some_and(|search| {
            search.needle == needle && search.attempt == self.directory_attempt
        }) {
            return;
        }
        let hex = needle.len() == 64 && needle.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex
            && self
                .refuted
                .as_ref()
                .is_some_and(|refuted| needle.starts_with(refuted.as_str()) && needle != *refuted)
        {
            self.directory = Some(DirectorySearch {
                needle,
                attempt: self.directory_attempt,
                result: Some(Ok((Vec::new(), false))),
                _task: None,
            });
            return;
        }
        let session = community.session.clone();
        let (attempt, wanted) = (self.directory_attempt, needle.clone());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let result = session.people(&wanted, 1).await.map(|page| {
                (
                    page.people
                        .iter()
                        .map(|p| p.pubkey.clone())
                        .collect::<Vec<_>>(),
                    page.more,
                )
            });
            let _ = this.update(cx, |this, cx| {
                let Some(search) = &mut this.directory else {
                    return;
                };
                if search.needle != wanted || search.attempt != attempt {
                    return;
                }
                if let Ok((people, more)) = &result {
                    this.recent_people = people.clone();
                    if people.is_empty() && !more && wanted.chars().any(char::is_alphanumeric) {
                        this.refuted = Some(wanted.clone());
                    }
                }
                if let Some(search) = &mut this.directory {
                    search.result = Some(result.map_err(|error| error.to_string()));
                }
                this.completion_key = None;
                cx.notify();
            });
        });
        self.directory = Some(DirectorySearch {
            needle,
            attempt,
            result: None,
            _task: Some(task),
        });
    }

    /// `@` rows, status line, retry and the Space pick (`MentionCompletion`).
    /// `None` when the query is prose, not a search.
    fn mention_suggestions(&self, query: &Query, cx: &App) -> Option<MentionRows> {
        let (teams, catalog) = self.saved_teams(cx);
        let team_names: Vec<String> = teams.iter().map(|(_, name, _)| name.clone()).collect();
        let found = self.mention_choices(&query.text, &team_names, cx);
        if found.closed {
            return None;
        }
        let community = self.community.read(cx);
        let channel = community
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel);
        let dm = channel.is_some_and(|c| c.channel_type == Some(ChannelType::Dm));
        let rows = self.team_suggestions(&teams, catalog, &query.text, cx);
        let people = found.people.len();
        let mut items: Vec<Suggestion> = found
            .people
            .into_iter()
            .map(|person| Suggestion::Person {
                pubkey: person.pubkey,
                name: person.name.into(),
                label: person.label.into(),
                picture: person.picture,
                agent: person.agent,
                member: person.member,
                dm,
                disabled: None,
            })
            .collect();
        let teams_shown = rows.len();
        // Teams follow the people, in one budget of 50 (`MentionCompletion`).
        items.truncate(mentions::LIMIT.saturating_sub(teams_shown));
        items.extend(rows.into_iter().take(mentions::LIMIT));
        let team_status = teams_status(catalog);
        let status = found
            .status
            .or_else(|| {
                (people + teams_shown > mentions::LIMIT)
                    .then_some("Narrow your search to see more members.")
            })
            .or(team_status);
        let retry = if found.retry {
            Some(channels::Retry::People)
        } else {
            (catalog == KitStatus::Error).then_some(channels::Retry::Teams)
        };
        Some(MentionRows {
            items,
            status: status.map(SharedString::from),
            retry,
            space: found
                .space
                .filter(|_| !mentions::teams_block_space(&team_names, &query.text)),
        })
    }

    /// Saved team rows for `query`, as `@` completion and the mention
    /// picker show them (`useTeamMentions`).
    pub(super) fn team_suggestions(
        &self,
        teams: &[(String, String, Vec<String>)],
        catalog: KitStatus,
        query: &str,
        cx: &App,
    ) -> Vec<Suggestion> {
        let community = self.community.read(cx);
        let channel = community
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel);
        let members: std::collections::HashSet<&str> = channel
            .map(|c| c.members.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let library = crate::ui::identity::AgentLibrary::from_hosts(cx);
        // Only agents the `@` choices offer here (`mentionCandidates`).
        let agents: std::collections::HashSet<&str> =
            if mentions::offers_agents(channel, !community.snapshot.cached) {
                library
                    .agents
                    .iter()
                    .map(|(key, _, _)| key.as_str())
                    .collect()
            } else {
                Default::default()
            };
        // Saved keys grant nothing: each agent must still be one this
        // draft can address (`mentionCandidates`).
        let rows = mentions::team_rows(
            teams,
            query,
            catalog == KitStatus::Ready,
            channel,
            |key| (members.contains(key) || agents.contains(key)) && self.admits(key, cx),
            |key| members.contains(key),
        );
        let profiles = &community.snapshot.profiles;
        let names = crate::ui::identity::names(
            profiles,
            &library,
            community.session.viewer(),
            Some(&community.origin),
            channel.map(|c| c.members.as_slice()),
        );
        rows.into_iter()
            .map(|team| Suggestion::Team {
                name: team.name.into(),
                recipients: team
                    .recipients
                    .iter()
                    .map(|key| {
                        let name = profiles
                            .get(key)
                            .map(|p| p.name.clone())
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| names.display(key));
                        let picture = profiles.get(key).and_then(|p| p.picture.clone());
                        (key.clone(), SharedString::from(name), picture)
                    })
                    .collect(),
                detail: team.detail.into(),
                disabled: team.disabled.is_some(),
                exact: false,
            })
            .collect()
    }

    /// The community's saved teams (`useTeamMentions`): id, name and
    /// agents, with the catalog's state.
    pub(super) fn saved_teams(&self, cx: &App) -> (Vec<(String, String, Vec<String>)>, KitStatus) {
        let kit = self.community.read(cx).session.kit().borrow().clone();
        let teams = kit
            .entries
            .iter()
            .filter(|entry| !entry.record.deleted)
            .filter_map(|entry| match &entry.record.value {
                KitValue::Team(team) => {
                    Some((team.id.clone(), team.name.clone(), team.agents.clone()))
                }
                _ => None,
            })
            .collect();
        (teams, kit.status)
    }

    /// Reads the saved teams and refreshes `@` completion as they change.
    pub(super) fn watch_teams(&mut self, cx: &mut Context<Self>) {
        if self.teams_watch.is_some() {
            return;
        }
        let session = self.community.read(cx).session.clone();
        session.kit_ensure();
        let mut kit = session.kit();
        self.teams_watch = Some(cx.spawn(async move |this, cx| {
            while kit.changed().await.is_ok() {
                let alive = this.update(cx, |this, cx| {
                    this.completion_key = None;
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// `mentionAdmission`: whether this draft may still address `pubkey`.
    /// Archived and read-only channels admit no one, archived identities
    /// are never admitted, and a session admits only its members and the
    /// viewer's agents.
    fn admits(&self, pubkey: &str, cx: &App) -> bool {
        let community = self.community.read(cx);
        let channel = community
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel);
        let Some(channel) = channel.filter(|c| !c.archived && !c.read_only) else {
            return false;
        };
        let hex = pubkey.len() == 64
            && pubkey
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex || super::archives::hides(&self.community, pubkey, cx) {
            return false;
        }
        mentions::searches_directory(Some(channel))
            || channel.members.iter().any(|m| m == pubkey)
            || crate::ui::identity::AgentLibrary::from_hosts(cx)
                .agents
                .iter()
                .any(|(key, _, _)| key == pubkey)
    }

    /// The `@` choices for `text`, ranked, with the directory search's state.
    fn mention_choices(&self, text: &str, teams: &[String], cx: &App) -> mentions::Completion {
        let community = self.community.read(cx);
        let snapshot = &community.snapshot;
        let channel = snapshot.channels.iter().find(|c| c.id == self.channel);
        let library = crate::ui::identity::AgentLibrary::from_hosts(cx);
        let history = cx
            .try_global::<MentionHistory>()
            .and_then(|h| h.0.get(&community.origin.http()))
            .map(|h| h.get(&self.channel))
            .unwrap_or_default();
        let directory = match &self.directory {
            None => mentions::Directory::Idle,
            Some(DirectorySearch {
                result: Some(Ok((people, more))),
                ..
            }) => mentions::Directory::Done {
                people,
                more: *more,
            },
            Some(DirectorySearch {
                result: Some(Err(_)),
                ..
            }) => mentions::Directory::Failed,
            Some(DirectorySearch { result: None, .. }) => {
                mentions::Directory::Loading(&self.recent_people)
            }
        };
        let inputs = mentions::Inputs {
            channel,
            roster_ready: snapshot.roster == RosterStatus::Ready,
            profiles: &snapshot.profiles,
            library: &library,
            viewer: community.session.viewer(),
            origin: Some(&community.origin),
            presence: &snapshot.presence,
            history: &history,
            teams,
            // Offline resume has no outbox to add members with.
            can_invite: !snapshot.cached,
        };
        let mut completion = mentions::complete(&inputs, text, directory);
        // Archived identities leave the choices (`archiveHides`).
        completion
            .people
            .retain(|person| !super::archives::hides(&self.community, &person.pubkey, cx));
        if completion
            .space
            .as_deref()
            .is_some_and(|key| super::archives::hides(&self.community, key, cx))
        {
            completion.space = None;
        }
        completion
    }

    /// Starts the open-channel lookup for `#query` after a typing pause,
    /// unless it already runs (`usePublicChannelSearch`).
    fn search_public(&mut self, query: &str, cx: &mut Context<Self>) {
        let needle = query.to_lowercase();
        let community = self.community.read(cx);
        if needle.trim().is_empty() || community.snapshot.roster != RosterStatus::Ready {
            self.public = None;
            return;
        }
        if self
            .public
            .as_ref()
            .is_some_and(|search| search.needle == needle && search.attempt == self.public_attempt)
        {
            return;
        }
        let session = community.session.clone();
        let (attempt, wanted) = (self.public_attempt, needle.clone());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PUBLIC_SEARCH_DELAY).await;
            let result = session.find_public_channels(&wanted, false).await;
            let _ = this.update(cx, |this, cx| {
                let Some(search) = &mut this.public else {
                    return;
                };
                if search.needle != wanted || search.attempt != attempt {
                    return;
                }
                if let Ok(found) = &result {
                    this.recent_public = found.channels.clone();
                }
                search.result = Some(result);
                // Recompute the popup with the answer.
                this.completion_key = None;
                cx.notify();
            });
        });
        self.public = Some(PublicSearch {
            needle,
            attempt,
            result: None,
            _task: task,
        });
    }

    /// `#` rows, status line and retry (`ChannelCompletion`).
    fn channel_suggestions(
        &self,
        query: &Query,
        cx: &App,
    ) -> (
        Vec<Suggestion>,
        Option<SharedString>,
        Option<channels::Retry>,
    ) {
        let community = self.community.read(cx);
        let snapshot = &community.snapshot;
        let needle = query.text.to_lowercase();
        // While the next lookup runs, the last answer's rows that still
        // match stay, so open channels don't flicker on every keystroke.
        let kept: Vec<ChannelSummary>;
        let search = match &self.public {
            None => channels::Search::Idle,
            Some(PublicSearch {
                result: Some(Ok(found)),
                ..
            }) => channels::Search::Done(found),
            Some(PublicSearch {
                result: Some(Err(_)),
                ..
            }) => channels::Search::Failed,
            Some(PublicSearch { result: None, .. }) => {
                kept = self
                    .recent_public
                    .iter()
                    .filter(|c| c.name.to_lowercase().contains(needle.trim()))
                    .cloned()
                    .collect();
                channels::Search::Loading(&kept)
            }
        };
        let completion = channels::complete(
            &snapshot.channels,
            community.session.viewer(),
            &snapshot.roster,
            &query.text,
            search,
            name_match,
        );
        let items = completion
            .items
            .into_iter()
            .map(|item| Suggestion::Channel {
                icon: channel_icon(&item.channel),
                id: item.channel.id,
                name: item.channel.name.into(),
                member: item.member,
                detail: item.detail.map(SharedString::from),
            })
            .collect();
        (
            items,
            completion.status.map(SharedString::from),
            completion.retry,
        )
    }

    /// `canSelect`: a joined row must still be joined under that name, an
    /// open row still open and unjoined.
    fn can_select(&self, id: &str, name: &str, member: bool, cx: &App) -> bool {
        let community = self.community.read(cx);
        let listed = community.snapshot.channels.iter().find(|c| c.id == id);
        if member {
            return listed.is_some_and(|c| {
                c.name == name && channels::joined(c, community.session.viewer())
            });
        }
        let found = match &self.public {
            Some(PublicSearch {
                result: Some(Ok(found)),
                ..
            }) => &found.channels,
            _ => &self.recent_public,
        };
        listed.is_none_or(|c| c.read_only)
            && found
                .iter()
                .any(|c| c.id == id && c.name == name && channels::open(c))
    }

    fn retry_completion(&mut self, retry: channels::Retry, cx: &mut Context<Self>) {
        match retry {
            channels::Retry::Channels => self.community.read(cx).session.retry(),
            channels::Retry::People => {
                self.directory_attempt += 1;
                self.directory = None;
                self.refuted = None;
                self.community.read(cx).session.retry();
            }
            channels::Retry::Search => {
                self.public_attempt += 1;
                self.public = None;
            }
            channels::Retry::Emoji => self.community.read(cx).session.refresh_emoji(),
            channels::Retry::Plugin(key) => {
                if let Some(completion) = super::composer_completions::get(key) {
                    (completion.retry)();
                }
            }
            channels::Retry::Teams => {
                let session = self.community.read(cx).session.clone();
                cx.background_spawn(async move {
                    let _ = session.kit_refresh().await;
                })
                .detach();
            }
        }
        self.completion_key = None;
        cx.notify();
    }

    fn suggestions(&self, query: &Query, cx: &App) -> Vec<Suggestion> {
        let community = self.community.read(cx);
        let snapshot = &community.snapshot;
        match query.trigger {
            // `mention_suggestions` owns `@`.
            Trigger::Mention => Vec::new(),
            // `channel_suggestions` owns `#`.
            Trigger::Channel => Vec::new(),
            Trigger::Plugin(_) => Vec::new(),
            Trigger::Emoji => {
                emoji_picker::search(&query.text, &snapshot.emoji.entries, EMOJI_LIMIT)
                    .into_iter()
                    .map(Suggestion::Emoji)
                    .collect()
            }
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(completion) = &mut self.completion
            && completion.count() > 0
        {
            let count = completion.count() as isize;
            completion.selected =
                ((completion.selected as isize + delta).rem_euclid(count)) as usize;
            self.completion_scroll.scroll_to_item(completion.selected);
            cx.notify();
        }
    }

    /// A space typed after an exact name, or the closing colon after an
    /// exact shortcode, accepts that one match (`spaceId`).
    fn accept_terminator(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(completion) = &mut self.completion else {
            return false;
        };
        let (text, selection) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        let query = &completion.query;
        let terminator = if query.trigger == Trigger::Emoji {
            ":"
        } else {
            " "
        };
        let typed = selection.is_empty()
            && selection.end == query.end + 1
            && text.get(query.end..query.end + 1) == Some(terminator)
            && text.get(query.start + 1..query.end) == Some(query.text.as_str());
        if !typed {
            return false;
        }
        // A terminator typed in code stays literal.
        if self.markdown.in_code(query.start..query.end) {
            return false;
        }
        if query.trigger == Trigger::Mention {
            // Space selects only the one exact name of a complete choice
            // set (`spaceId`).
            let Some(space) = completion.space.clone() else {
                return false;
            };
            let Some(index) = completion.items.iter().position(
                |item| matches!(item, Suggestion::Person { pubkey, .. } if *pubkey == space),
            ) else {
                return false;
            };
            completion.query.end += 1;
            self.accept(index, window, cx);
            return true;
        }
        if matches!(query.trigger, Trigger::Plugin(_)) {
            // The plugin names the one item a space picks (`spaceId`).
            let Some(index) = completion.items.iter().position(|item| {
                matches!(
                    item,
                    Suggestion::Text {
                        exact: true,
                        disabled: None,
                        ..
                    } | Suggestion::Team {
                        exact: true,
                        disabled: false,
                        ..
                    }
                )
            }) else {
                return false;
            };
            completion.query.end += 1;
            self.accept(index, window, cx);
            return true;
        }
        // Automatic replacement needs both catalogs (`spaceId`).
        if query.trigger == Trigger::Emoji
            && self.community.read(cx).snapshot.emoji.status
                != buzz_relay_client::CatalogStatus::Ready
        {
            return false;
        }
        let wanted = query.text.trim().to_lowercase();
        let exact: Vec<usize> = completion
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| match item {
                Suggestion::Person { name, .. } | Suggestion::Channel { name, .. } => {
                    name.to_lowercase() == wanted
                }
                Suggestion::Emoji(candidate) => candidate.shortcode().to_lowercase() == wanted,
                Suggestion::Team { .. } | Suggestion::Text { .. } => false,
            })
            .map(|(index, _)| index)
            .collect();
        let [index] = exact[..] else {
            return false;
        };
        completion.query.end += 1;
        self.accept(index, window, cx);
        true
    }

    fn accept(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = &self.completion else {
            return;
        };
        if index == completion.items.len()
            && let Some(retry) = completion.retry
        {
            self.retry_completion(retry, cx);
            return;
        }
        if let Some(Suggestion::Channel {
            id, name, member, ..
        }) = completion.items.get(index)
            && !self.can_select(id, name, *member, cx)
        {
            return;
        }
        if let Some(
            Suggestion::Person {
                disabled: Some(_), ..
            }
            | Suggestion::Text {
                disabled: Some(_), ..
            },
        ) = completion.items.get(index)
        {
            return;
        }
        if let Some(Suggestion::Person { pubkey, .. }) = completion.items.get(index)
            && self.editing.is_none()
        {
            if !self.admits(pubkey, cx) {
                self.error = Some(
                    "This recipient is no longer available. Remove it or refresh choices.".into(),
                );
                cx.notify();
                return;
            }
            let recipients = self.recipients(cx);
            if recipients.len() >= 32 && !recipients.iter().any(|(key, _)| key == pubkey) {
                self.error = Some("Choose at most 32 recipients".into());
                cx.notify();
                return;
            }
            let community = self.community.read(cx);
            let (origin, channel) = (community.origin.http(), self.channel.clone());
            let pubkey = pubkey.clone();
            cx.default_global::<MentionHistory>()
                .0
                .entry(origin)
                .or_default()
                .remember(&channel, &pubkey);
        }
        if let Some(Suggestion::Team {
            recipients,
            disabled,
            ..
        }) = self.completion.as_ref().and_then(|c| c.items.get(index))
        {
            // A disabled team takes Enter and Tab without sending.
            if *disabled {
                return;
            }
            let recipients = recipients.clone();
            self.accept_team(recipients, window, cx);
            return;
        }
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.items.get(index) else {
            return;
        };
        let range = completion.query.start..completion.query.end;
        let token = match item {
            // While editing, a pick is an identity link, never a recipient.
            Suggestion::Person { pubkey, name, .. } if self.editing.is_some() => {
                message_edit::mention_text(pubkey, name).map(|link| {
                    InlineToken::new(pubkey.clone(), link).with_label(format!("@{name}"))
                })
            }
            Suggestion::Person { pubkey, name, .. } => {
                Some(InlineToken::new(pubkey.clone(), format!("@{name}")))
            }
            Suggestion::Channel { id, name, .. } => Some(
                InlineToken::new(id.clone(), channel_reference(id, name))
                    .with_label(format!("#{name}")),
            ),
            Suggestion::Emoji(_) | Suggestion::Team { .. } | Suggestion::Text { .. } => None,
        };
        let text = match item {
            Suggestion::Emoji(candidate) => candidate.pick(None).text(),
            Suggestion::Team { name, .. } => format!("@{name} "),
            Suggestion::Person { name, .. } => format!("@{name} "),
            Suggestion::Channel { id, name, .. } => format!("{} ", channel_reference(id, name)),
            // A space after it unless it is only emoji (`replaceCompletion`).
            Suggestion::Text { text, .. } => {
                let catalog = self.community.read(cx).snapshot.emoji.entries.clone();
                if super::markdown::is_emoji_only_by(text, |code| {
                    catalog.iter().any(|entry| entry.shortcode == code)
                }) {
                    text.clone()
                } else {
                    format!("{text} ")
                }
            }
        };
        self.input.update(cx, |state, cx| {
            let chip = token.and_then(|token| {
                state
                    .replace_range_with_token(range.clone(), token, window, cx)
                    .ok()
            });
            if chip.is_some() {
                state.insert(" ", window, cx);
            } else {
                state.set_selected_range(range.clone(), cx);
                state.replace(text, window, cx);
            }
            state.focus(window, cx);
        });
        self.completion = None;
        cx.notify();
    }

    /// A saved team's pick: a chip for each of its agents, each a
    /// recipient (`edit: { mentions }`).
    fn accept_team(
        &mut self,
        recipients: Vec<(String, SharedString, Option<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let range = completion.query.start..completion.query.end;
        if self.insert_team(&recipients, Some(range), window, cx) {
            self.completion = None;
        } else {
            self.completion = Some(completion);
        }
        cx.notify();
    }

    /// Puts a chip for each of a team's agents over `range`, or at the
    /// caret. False when that would pass 32 recipients.
    pub(crate) fn insert_team(
        &mut self,
        recipients: &[(String, SharedString, Option<String>)],
        range: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Everyone must still be mentionable in this conversation
        // (`mentionAdmission`); a draft not in one yet keeps its own roster.
        if self.editing.is_none()
            && !self.channel.is_empty()
            && recipients.iter().any(|(key, _, _)| !self.admits(key, cx))
        {
            self.error = Some(UNAVAILABLE_MEMBER.into());
            cx.notify();
            return false;
        }
        // An edit names people with identity links, never recipients.
        let editing = self.editing.is_some();
        let chosen = self.recipients(cx);
        let added = recipients
            .iter()
            .filter(|(key, _, _)| !chosen.iter().any(|(have, _)| have == key))
            .count();
        if !editing && chosen.len() + added > 32 {
            self.error = Some("Choose at most 32 recipients".into());
            return false;
        }
        let community = self.community.read(cx);
        let (origin, channel) = (community.origin.http(), self.channel.clone());
        let history = cx
            .default_global::<MentionHistory>()
            .0
            .entry(origin)
            .or_default();
        for (key, _, _) in recipients {
            history.remember(&channel, key);
        }
        self.input.update(cx, |state, cx| {
            state.focus(window, cx);
            let range = range.unwrap_or_else(|| state.selected_range());
            for (index, (key, name, _)) in recipients.iter().enumerate() {
                let token = match editing {
                    true => match message_edit::mention_text(key, name) {
                        Some(link) => {
                            InlineToken::new(key.clone(), link).with_label(format!("@{name}"))
                        }
                        None => continue,
                    },
                    false => InlineToken::new(key.clone(), format!("@{name}")),
                };
                let placed = if index == 0 {
                    state.replace_range_with_token(range.clone(), token, window, cx)
                } else {
                    state.insert(" ", window, cx);
                    state.replace_with_token(token, window, cx)
                };
                if placed.is_err() {
                    break;
                }
            }
            state.insert(" ", window, cx);
        });
        true
    }

    // ----- formatting -----

    fn format(&mut self, format: Format, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending || self.edit_status.locked {
            return;
        }
        let (text, selection) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        if format == Format::Link {
            self.open_link(window, cx);
            return;
        }
        let Some(kind) = format.kind() else {
            self.leave_pending(window, cx);
            let (text, selection) = {
                let state = self.input.read(cx);
                (state.value().to_string(), state.selected_range())
            };
            let edit = format::apply(&text, selection, format);
            self.apply_edit(edit, window, cx);
            return;
        };
        // Code stays literal for every other format (`markdownRanges`).
        if kind != live_markdown::CODE
            && self.markdown.pairs(live_markdown::CODE).iter().any(|pair| {
                pair.open.start < selection.end.max(selection.start + 1)
                    && pair.close.end > selection.start
            })
        {
            return;
        }
        let active = self.active_formats(cx);
        let has = |active: &live_markdown::Active| match format {
            Format::Bold => active.bold,
            Format::Italic => active.italic,
            Format::Strike => active.strike,
            Format::Code => active.code,
            _ => active.spoiler,
        };
        if selection.is_empty() && format == Format::Spoiler && !text.trim().is_empty() {
            // A collapsed caret puts the whole draft behind a spoiler.
            self.leave_pending(window, cx);
            let state = self.input.read(cx);
            let (text, caret) = (state.value().to_string(), state.selected_range().end);
            let whole = 0..text.len();
            let all = self.markdown.active(whole.clone(), &text, 0).spoiler;
            let edit = format::toggle_range(&text, whole, format, &self.markdown.pairs(kind), all);
            let caret = format::map_offset(caret, &edit.changes);
            self.apply_edit(
                format::Edit {
                    changes: edit.changes,
                    selection: caret..caret,
                },
                window,
                cx,
            );
            return;
        }
        if selection.is_empty() {
            if let Some(index) = self.pending.iter().position(|p| p.kind == kind) {
                // Toggling a pending format again takes it back.
                let ranges = self.pending_ranges();
                let open = ranges[index].clone();
                let close = ranges[ranges.len() - 1 - index].clone();
                self.pending.remove(index);
                let caret = self.pending_at - open.len();
                self.pending_at = caret;
                self.apply_edit(
                    format::Edit {
                        changes: vec![(open, String::new()), (close, String::new())],
                        selection: caret..caret,
                    },
                    window,
                    cx,
                );
                return;
            }
            if !self.pending.is_empty() && !self.pending_holds(&text, selection.end) {
                self.pending.clear();
            }
            let inside = has(&self.markdown.active(selection.clone(), &text, 0));
            let (edit, marker) = format::pending_pair(&text, selection.end, format, inside);
            self.pending.push(PendingFormat {
                marker,
                kind,
                removes: inside,
            });
            self.pending_at = edit.selection.end;
            self.apply_edit(edit, window, cx);
            return;
        }
        self.leave_pending(window, cx);
        let edit = format::toggle_range(
            &text,
            selection,
            format,
            &self.markdown.pairs(kind),
            has(&active),
        );
        self.apply_edit(edit, window, cx);
    }

    /// "Add link" / "Edit link" on the selection, Mod+K or the toolbar.
    fn open_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending || self.edit_status.locked || self.input_disabled(cx) {
            return;
        }
        self.leave_pending(window, cx);
        let (text, selection) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        // Code stays literal.
        if self
            .markdown
            .pairs(live_markdown::CODE)
            .iter()
            .any(|pair| pair.open.start <= selection.start && selection.end <= pair.close.end)
        {
            return;
        }
        let target = link::target(
            &text,
            selection,
            &self.markdown.pairs(live_markdown::LINK),
            &self.markdown.bare_links(),
        );
        let dialog = cx.new(|cx| link::LinkDialog::new(target, window, cx));
        self._subscriptions.push(cx.subscribe_in(
            &dialog,
            window,
            |this, dialog, event: &link::LinkEvent, window, cx| {
                this.link_event(dialog, event, window, cx)
            },
        ));
        self.link = Some((dialog, text));
        cx.notify();
    }

    fn link_event(
        &mut self,
        dialog: &Entity<link::LinkDialog>,
        event: &link::LinkEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, opened)) = &self.link else {
            return;
        };
        let current = self.input.read(cx).value().to_string();
        let target = dialog.read(cx).target.clone();
        let replacement = match event {
            link::LinkEvent::Close => {
                self.close_link(window, cx);
                return;
            }
            link::LinkEvent::Save { text, href } => link::markdown(text, href),
            link::LinkEvent::Remove => target.text.clone(),
        };
        let too_long = current.chars().count() - current[target.range.clone()].chars().count()
            + replacement.chars().count()
            > MAX_CHARS;
        if current != *opened || too_long {
            let error = match event {
                link::LinkEvent::Remove => "The draft changed. Close and try again.",
                _ => {
                    "The draft changed or this link exceeds the message limit. Close and try again."
                }
            };
            dialog.update(cx, |dialog, cx| {
                dialog.error = Some(error.into());
                cx.notify();
            });
            return;
        }
        let end = target.range.start + replacement.len();
        self.apply_edit(
            format::Edit {
                changes: vec![(target.range, replacement)],
                selection: end..end,
            },
            window,
            cx,
        );
        self.close_link(window, cx);
    }

    fn close_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.link = None;
        self.focus(window, cx);
        cx.notify();
    }

    /// Runs a block key (`blocks`) at the selection; whether it applied.
    fn block_edit(
        &mut self,
        key: fn(&str, std::ops::Range<usize>) -> Option<format::Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.sending || self.edit_status.locked {
            return false;
        }
        let edit = {
            let state = self.input.read(cx);
            key(state.value().as_ref(), state.selected_range())
        };
        match edit {
            Some(edit) => {
                self.apply_edit(edit, window, cx);
                true
            }
            None => false,
        }
    }

    /// Applies a formatting edit as one change to the field.
    fn apply_edit(&mut self, edit: format::Edit, window: &mut Window, cx: &mut Context<Self>) {
        self.applying_format = true;
        self.input.update(cx, |state, cx| {
            for (range, replacement) in edit.changes.iter().rev() {
                state.set_selected_range(range.clone(), cx);
                state.replace(replacement.clone(), window, cx);
            }
            state.set_selected_range(edit.selection, cx);
            state.focus(window, cx);
        });
        self.applying_format = false;
        self.restyle(cx);
        cx.notify();
    }

    fn shortcut(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if !modifiers.secondary() || modifiers.function {
            return;
        }
        let key = keystroke.key.to_lowercase();
        let format = match (modifiers.shift, modifiers.alt, key.as_str()) {
            (false, false, "b") => Format::Bold,
            (false, false, "i") => Format::Italic,
            (false, false, "e") => Format::Code,
            (true, false, "x") => Format::Strike,
            (true, false, "p") => Format::Spoiler,
            (false, true, "c") => Format::CodeBlock,
            (true, false, "8" | "*") => Format::Bullet,
            (true, false, "7" | "&") => Format::Ordered,
            (true, false, "b") => Format::Quote,
            _ => return,
        };
        cx.stop_propagation();
        if !event.is_held {
            self.format(format, window, cx);
        }
    }

    // ----- attachments -----

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Development only: a headless session has no file dialog. The
        // variable may list several files, as `PATH` does.
        if let Some(files) = std::env::var_os("BUZZ_DEV_PICK_FILE") {
            self.add_paths(std::env::split_paths(&files).collect(), cx);
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| this.add_paths(paths, cx));
            }
        })
        .detach();
    }

    /// Files chosen, dropped or pasted (`attachFiles`). Refusals show the
    /// "Could not attach file" toast.
    fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let read = cx
            .background_executor()
            .spawn(async move { attachments::files_from_paths(paths) });
        cx.spawn(async move |this, cx| {
            let files = read.await;
            let _ = this.update(cx, |this, cx| {
                let mut ok = Vec::new();
                for file in files {
                    match file {
                        Ok(file) => ok.push(file),
                        Err((_, error)) => {
                            this.attach_error = Some(error.into());
                            cx.notify();
                            return;
                        }
                    }
                }
                this.add_files(ok, cx);
            });
        })
        .detach();
    }

    fn set_drag_over(&mut self, over: bool, cx: &mut Context<Self>) {
        let over = over && self.accepts_drop();
        if self.drag_over != over {
            self.drag_over = over;
            cx.notify();
        }
    }

    /// Dropping files works outside edits and voice capture (`useFileDrop`).
    fn accepts_drop(&self) -> bool {
        self.editing.is_none() && self.capture.is_none() && !self.sending
    }

    fn add_files(&mut self, files: Vec<NewFile>, cx: &mut Context<Self>) {
        if files.is_empty() || self.capture.is_some() || self.sending {
            return;
        }
        if self.editing.is_some() {
            self.attach_error = Some("Finish editing before attaching new files.".into());
            cx.notify();
            return;
        }
        match attachments::add(&self.community, &self.files, files, cx) {
            Ok(()) => self.attach_error = None,
            Err(error) => self.attach_error = Some(error.into()),
        }
        cx.notify();
    }

    // ----- voice notes -----

    /// "Record voice note": starts the microphone at once
    /// (`useVoiceCapture`), unless a note is already attached or the
    /// message is full.
    fn start_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let files = self.files.read(cx);
        if self.editing.is_some()
            || self.sending
            || self.capture.is_some()
            || files.items.iter().any(|item| item.voice.is_some())
        {
            return;
        }
        if files.items.len() >= MAX_FILES {
            self.attach_error =
                Some(format!("Attach at most {MAX_FILES} files per message.").into());
            cx.notify();
            return;
        }
        self.capture = Some(match Recorder::start() {
            Ok(recorder) => Capture::Recording(recorder),
            Err(error) => Capture::Failed(error.into()),
        });
        // Discard takes focus on start, Close on a failure.
        window.focus(&self.capture_focus, cx);
        self._capture_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let going = this.update_in(cx, |this, window, cx| {
                    let Some(Capture::Recording(recorder)) = &this.capture else {
                        return false;
                    };
                    if let Some(error) = recorder.failure() {
                        this.capture = Some(Capture::Failed(error.into()));
                    } else if recorder.seconds() >= recorder::MAX_SECONDS {
                        // The five-minute cap finishes the note.
                        this.finish_capture(window, cx);
                    }
                    cx.notify();
                    true
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Shows a plugin tool's capture view in place of the editor
    /// (`capture`), as Buzz's own recorder starts (`voice.begin`): not
    /// while another capture, an edit or a send is under way, when the
    /// composer takes no files, or when a voice note is already attached
    /// or the message is full. The error is why not.
    pub fn start_plugin_capture(
        &mut self,
        view: AnyView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(refusal) = self.capture_refusal(cx) {
            return Err(refusal);
        }
        self.capture = Some(Capture::Plugin(view));
        window.focus(&self.capture_focus, cx);
        cx.notify();
        Ok(())
    }

    /// Why a plugin capture cannot start now, if it cannot: the checks
    /// of [`Self::start_plugin_capture`], made before its view opens. A
    /// full message also shows Buzz's notice.
    pub fn capture_refusal(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let files = self.files.read(cx);
        if self.capture.is_some() || self.editing.is_some() || self.sending {
            return Some("The composer is busy.".to_owned());
        }
        if self.no_attachments {
            return Some("This composer takes no attachments.".to_owned());
        }
        if files.items.iter().any(|item| item.voice.is_some()) {
            return Some("A voice note is already attached.".to_owned());
        }
        if files.items.len() >= MAX_FILES {
            let full = format!("Attach at most {MAX_FILES} files per message.");
            self.attach_error = Some(full.clone().into());
            cx.notify();
            return Some(full);
        }
        None
    }

    /// Whether the plugin capture showing is the view `capture`.
    fn capturing(&self, capture: EntityId) -> bool {
        matches!(&self.capture, Some(Capture::Plugin(view)) if view.entity_id() == capture)
    }

    /// Ends the plugin capture `capture` with nothing attached, as when
    /// the tool that started it goes. Another capture is left alone.
    pub fn cancel_plugin_capture(&mut self, capture: EntityId, cx: &mut Context<Self>) {
        if self.capturing(capture) {
            self.capture = None;
            cx.notify();
        }
    }

    /// Ends the plugin capture `capture` (`accept`), attaching `file` if
    /// it made one. `false` when that capture is not showing, or the
    /// composer was disabled meanwhile, which ends it with nothing.
    pub fn accept_plugin_capture(
        &mut self,
        capture: EntityId,
        file: Option<Captured>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.capturing(capture) {
            return false;
        }
        self.capture = None;
        if self.disabled(cx) {
            cx.notify();
            return false;
        }
        if let Some(file) = file {
            use buzz_relay_client::upload::Source;
            // A recording plays from memory until it is sent, as Buzz's own.
            let voice = file.voice.and_then(|(duration, waveform)| {
                let bytes: Arc<[u8]> = match &file.source {
                    Source::Bytes(bytes) => bytes.clone(),
                    Source::Path(path) => std::fs::read(path).ok()?.into(),
                };
                let key = format!("voice-draft:{}", uuid::Uuid::new_v4());
                media::set_local(&key, bytes, cx);
                Some(Voice {
                    key,
                    duration,
                    waveform,
                })
            });
            self.add_files(
                vec![NewFile {
                    name: file.name,
                    mime: file.mime,
                    size: file.size,
                    source: file.source,
                    voice,
                }],
                cx,
            );
        }
        self.focus(window, cx);
        cx.notify();
        true
    }

    fn discard_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.capture, Some(Capture::Preparing)) {
            return;
        }
        self.capture = None;
        self._capture_task = None;
        self.focus(window, cx);
        cx.notify();
    }

    /// "Finish voice note": stops, encodes, and attaches the note.
    fn finish_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Capture::Recording(recorder)) = self.capture.take() else {
            return;
        };
        if !recorder.live() {
            self.capture = Some(Capture::Recording(recorder));
            return;
        }
        self.capture = Some(Capture::Preparing);
        let prepared = cx.background_executor().spawn(async move {
            let samples = recorder.finish();
            let duration = recorder::duration(&samples).min(recorder::MAX_SECONDS);
            let waveform = recorder::waveform(&samples, recorder::WAVEFORM_BUCKETS);
            recorder::encode_mp4(&samples).map(|mp4| (mp4, duration, waveform))
        });
        self._capture_task = Some(cx.spawn_in(window, async move |this, cx| {
            let prepared = prepared.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match prepared {
                    Ok((mp4, duration, waveform)) if duration > 0. => {
                        this.capture = None;
                        this.add_voice(mp4, duration, waveform, cx);
                        this.focus(window, cx);
                    }
                    _ => {
                        this.capture = Some(Capture::Failed(
                            "Could not attach this voice note. Close and try again.".into(),
                        ))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn add_voice(
        &mut self,
        mp4: Vec<u8>,
        duration: f64,
        waveform: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let key = format!("voice-draft:{}", uuid::Uuid::new_v4());
        let bytes: Arc<[u8]> = mp4.into();
        media::set_local(&key, bytes.clone(), cx);
        let file = NewFile {
            name: format!("voice-note-{millis}.mp4"),
            mime: "video/mp4".into(),
            size: bytes.len() as u64,
            source: Source::Bytes(bytes),
            voice: Some(Voice {
                key,
                duration,
                waveform,
            }),
        };
        if let Err(error) = attachments::add(&self.community, &self.files, vec![file], cx) {
            self.attach_error = Some(error.into());
        }
        cx.notify();
    }

    fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        let found = {
            let items = &self.files.read(cx).items;
            items
                .iter()
                .position(|item| item.id == id)
                .map(|index| (index, items[index].clone()))
        };
        let voice = found.as_ref().and_then(|(_, item)| item.voice.clone());
        if let Some((index, item)) = found
            && item.voice.is_none()
        {
            self.exiting.push((index, item, Instant::now()));
        }
        if let Some(voice) = voice {
            media::forget_local(&voice.key, cx);
        }
        self.files.update(cx, |files, cx| files.remove(id, cx));
        cx.notify();
    }

    fn retry_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        self.files.update(cx, |files, cx| files.retry(id, cx));
        cx.notify();
    }

    /// Images and files from the clipboard attach; text pastes through
    /// [`Self::paste_text`].
    fn paste(&mut self, item: &ClipboardItem, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut images = Vec::new();
        let mut paths = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let extension = match image.format {
                        ImageFormat::Png => "png",
                        ImageFormat::Jpeg => "jpg",
                        ImageFormat::Gif => "gif",
                        ImageFormat::Webp => "webp",
                        _ => "img",
                    };
                    images.push(attachments::file_from_bytes(
                        format!("image.{extension}"),
                        image.bytes.clone(),
                    ));
                }
                ClipboardEntry::ExternalPaths(external) => {
                    paths.extend(external.paths().iter().cloned());
                }
                ClipboardEntry::String(_) => {}
            }
        }
        let consumed = !images.is_empty() || !paths.is_empty();
        if !images.is_empty() {
            self.add_files(images, cx);
        }
        if !paths.is_empty() {
            self.add_paths(paths, cx);
        }
        if consumed {
            return true;
        }
        self.paste_text(item, window, cx)
    }

    /// Pastes text (`paste` in `EditableInput.tsx`): this app's own copy
    /// brings back its links, other text pastes as itself, and code takes
    /// what is visible verbatim. A lone link gets a trailing space, and
    /// pasted identity links become recipients when they are admitted (at
    /// most 32), otherwise name chips, as channel links become channel
    /// chips.
    fn paste_text(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.input_disabled(cx) {
            return true;
        }
        let (current, selection) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        let in_code = self
            .markdown
            .pairs(live_markdown::CODE)
            .iter()
            .any(|pair| pair.open.end <= selection.start && selection.end <= pair.close.start);
        let copied = item
            .metadata()
            .and_then(|json| serde_json::from_str::<clipboard::Copied>(json).ok());
        let mut text = match copied {
            Some(copied) if !in_code => copied.buzz_composer,
            _ => match item.text() {
                Some(text) => text,
                None => return false,
            },
        };
        if text.is_empty() {
            return true;
        }
        let link = !in_code && clipboard::lone_link(&text);
        let reuse = link && current[selection.end..].starts_with(' ');
        if link && !reuse {
            text.push(' ');
        }
        let start = selection.start;
        self.input.update(cx, |state, cx| {
            state.replace(text.clone(), window, cx);
            if reuse {
                let at = start + text.len() + 1;
                state.set_selected_range(at..at, cx);
            }
        });
        if !in_code {
            self.chip_pasted(start, &text, window, cx);
        }
        true
    }

    /// Turns the identity and channel links pasted at `start` into chips.
    fn chip_pasted(
        &mut self,
        start: usize,
        pasted: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = edit_content(pasted);
        if found.tokens().is_empty() {
            return;
        }
        // The pasted text must still be where it went (the limit may have
        // refused it).
        if self.input.read(cx).value().get(start..start + pasted.len()) != Some(pasted) {
            return;
        }
        let (members, dm) = {
            let channel = self.summary(cx);
            (
                channel.map(|c| c.members.clone()).unwrap_or_default(),
                channel.is_some_and(|c| c.channel_type == Some(ChannelType::Dm)),
            )
        };
        let mut recipients = self.recipients(cx).len();
        let caret = self.input.read(cx).selected_range();
        let mut shift: isize = 0;
        let mut chips: Vec<(std::ops::Range<usize>, InlineToken)> = Vec::new();
        for span in found.tokens() {
            let range = span.range().start + start..span.range().end + start;
            let token = span.token().clone();
            let identity = token.text().starts_with("[\\@");
            // A recipient needs room, the channel's membership (outside a
            // DM, where anyone is a reference), and a mention boundary.
            let next = pasted[span.range().end..].chars().next();
            let boundary = next.is_none_or(|c| c.is_whitespace() || c.is_ascii_punctuation());
            let admitted = identity
                && recipients < 32
                && boundary
                && !dm
                && (members.is_empty()
                    || members.iter().any(|m| m.as_str() == token.id().as_ref()));
            if admitted {
                recipients += 1;
                chips.push((
                    range,
                    InlineToken::new(token.id().clone(), token.label().to_string()),
                ));
            } else {
                chips.push((range, token));
            }
        }
        self.applying_format = true;
        self.input.update(cx, |state, cx| {
            for (range, token) in chips.iter().rev() {
                let before = range.len() as isize;
                let after = token.text().len() as isize;
                if state
                    .replace_range_with_token(range.clone(), token.clone(), window, cx)
                    .is_ok()
                {
                    shift += after - before;
                }
            }
            let at = (caret.end as isize + shift).max(0) as usize;
            state.set_selected_range(at..at, cx);
        });
        self.applying_format = false;
        self.restyle(cx);
    }

    /// Copy and Cut: the Markdown with chips as their names, plus the
    /// links for this app's own paste.
    fn copy(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (content, selection) = {
            let state = self.input.read(cx);
            (state.content(), state.selected_range())
        };
        if selection.is_empty() {
            return false;
        }
        let (text, copied) = clipboard::copy(&content, selection);
        cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(text, copied));
        if cut && !self.input_disabled(cx) {
            self.input
                .update(cx, |state, cx| state.replace("", window, cx));
        }
        true
    }
}

/// Makes a whole conversation pane the composer's drop zone
/// (`data-attachment-drop-zone`): files dropped anywhere in it attach to
/// this composer, which shows the drop state.
pub fn drop_zone(pane: Div, composer: &Entity<Composer>, cx: &mut App) -> Div {
    composer.update(cx, |composer, _| composer.zoned = true);
    let (moving, dropped) = (composer.downgrade(), composer.downgrade());
    pane.on_drag_move(move |event: &DragMoveEvent<ExternalPaths>, _, cx| {
        let over = event.bounds.contains(&event.event.position);
        let _ = moving.update(cx, |composer, cx| composer.set_drag_over(over, cx));
    })
    .on_drop(move |paths: &ExternalPaths, _, cx| {
        let _ = dropped.update(cx, |composer, cx| {
            composer.drag_over = false;
            if composer.accepts_drop() {
                composer.add_paths(paths.paths().to_vec(), cx);
            }
            cx.notify();
        });
    })
}

/// "Drop files into this conversation" over the drop zone while files
/// are dragged over it (`FileDropOverlay`). The zone must be `relative`.
pub fn drop_overlay(composer: &Composer, cx: &App) -> Option<AnyElement> {
    if !composer.drag_over {
        return None;
    }
    let c = cx.colors();
    Some(
        div()
            .absolute()
            .inset(px(8.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .p(px(16.))
            .border_2()
            .border_dashed()
            .border_color(c.border_focus)
            .rounded(px(20.))
            .bg(c.surface_panel)
            .opacity(0.95)
            .text_label()
            .text_color(c.text_standard)
            .text_center()
            .child(Icon::new(IconName::CloudUpload).size(32.))
            .child("Drop files into this conversation")
            .into_any_element(),
    )
}

/// The drop overlay of `composer`'s pane.
pub fn pane_drop_overlay(composer: &Entity<Composer>, cx: &App) -> Option<AnyElement> {
    drop_overlay(composer.read(cx), cx)
}

/// `draft:<channel>`, or `draft:<channel>:thread:<root>` for a thread.
/// `tools_generation` before the composer first shows.
const UNBUILT: u64 = u64::MAX;

fn draft_key(channel: &str, thread_root: Option<EventId>) -> String {
    match thread_root {
        Some(root) => format!("draft:{channel}:thread:{}", root.to_hex()),
        None => format!("draft:{channel}"),
    }
}

/// Whether a saved draft holds no text or chips.
pub(crate) fn draft_is_empty(json: &str) -> bool {
    serde_json::from_str::<Draft>(json).is_ok_and(|draft| draft.text.trim().is_empty())
}

/// The text to edit, with its identity links and channel references back
/// as chips.
fn edit_content(text: &str) -> InputContent {
    const LINK: &str = "](buzz://channel/";
    let mut content = InputContent::new(text.to_string());
    // `[\@Name](nostr:npub1…)`, as `profileMentionLink` writes them.
    let mut from = 0;
    while let Some(open) = text[from..].find("[\\@").map(|i| from + i) {
        from = open + 3;
        let Some(link) = text[open..].find("](nostr:npub1").map(|i| open + i) else {
            break;
        };
        let Some(close) = text[link..].find(')').map(|i| link + i) else {
            break;
        };
        let mut name = String::new();
        let mut escaped = false;
        for c in text[open + 1..link].chars() {
            if c == '\\' && !escaped {
                escaped = true;
            } else {
                name.push(c);
                escaped = false;
            }
        }
        let Some(key) = buzz_relay_client::profile_key(&text[link + 2..close]) else {
            continue;
        };
        let chip = InlineToken::new(key.to_hex(), text[open..=close].to_string()).with_label(name);
        if let Ok(next) = content.clone().with_token(open..close + 1, chip) {
            content = next;
        }
        from = close + 1;
    }
    let mut from = 0;
    while let Some(open) = text[from..].find("[\\#").map(|i| from + i) {
        from = open + 3;
        let Some(link) = text[open..].find(LINK).map(|i| open + i) else {
            break;
        };
        let Some(close) = text[link..].find(')').map(|i| link + i) else {
            break;
        };
        let label = &text[open + 3..link];
        let id = &text[link + LINK.len()..close];
        let name: String = {
            let mut name = String::new();
            let mut escaped = false;
            for c in label.chars() {
                if c == '\\' && !escaped {
                    escaped = true;
                } else {
                    name.push(c);
                    escaped = false;
                }
            }
            name
        };
        let id = url::form_urlencoded::parse(format!("x={id}").as_bytes())
            .next()
            .map(|(_, id)| id.into_owned())
            .unwrap_or_default();
        if name.is_empty() || id.is_empty() || channel_reference(&id, &name) != text[open..=close] {
            continue;
        }
        let chip =
            InlineToken::new(id, text[open..=close].to_string()).with_label(format!("#{name}"));
        if let Ok(next) = content.clone().with_token(open..close + 1, chip) {
            content = next;
        }
        from = close + 1;
    }
    content
}

/// `[#name](buzz://channel/<id>)`, as `ChannelCompletion` inserts it.
fn channel_reference(id: &str, name: &str) -> String {
    let mut label = String::from("\\#");
    for c in name.chars() {
        if "\\`*_[]()#<>~&".contains(c) {
            label.push('\\');
        }
        label.push(c);
    }
    let id: String = url::form_urlencoded::byte_serialize(id.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("[{label}](buzz://channel/{id})")
}

/// Whether the draft is emoji only, Unicode or the community's
/// (`usesLargeEmojiPresentation`).
fn large_emoji(text: &str, entries: &[buzz_relay_client::CustomEmoji]) -> bool {
    // Prose is never emoji only; skip the catalog for it.
    if text.trim().is_empty() || (text.chars().any(char::is_alphabetic) && !text.contains(':')) {
        return false;
    }
    crate::ui::markdown::is_emoji_only_by(text, |code| {
        entries.iter().any(|entry| entry.shortcode == code)
    })
}

/// A spring with a little bounce (`type: "spring", bounce: 0.15`).
fn spring(t: f32) -> f32 {
    let (c1, c3) = (0.7f32, 1.7f32);
    1. + c3 * (t - 1.).powi(3) + c1 * (t - 1.).powi(2)
}

/// The toolbar's reveal: 140ms ease-out, or none (`reveal`).
fn reveal(instant: bool) -> Animation {
    Animation::new(Duration::from_millis(if instant { 1 } else { 140 }))
        .with_easing(|t: f32| 1. - (1. - t).powi(4))
}

/// The composer's corners: the send control's (10) plus its inset (6),
/// measured from the outer border.
const COMPOSER_RADIUS: f32 = 17.;
/// A media tile's side: twice a file chip's height (40 + 2 × 2 + 2).
const MEDIA_TILE: f32 = 92.;
/// The composer's outer curve across its padding and border (20 - 6 - 1;
/// buzz-app keeps the panel radius here).
const MEDIA_RADIUS: f32 = 13.;
/// How long a removed tile takes to leave.
const TILE_EXIT: Duration = Duration::from_millis(180);

/// `--easing-enter`.
fn ease_enter(t: f32) -> f32 {
    buzz_ui::tokens::easing::cubic_bezier(buzz_ui::tokens::easing::ENTER, t)
}

/// A tile's 20px round control (`IconButton size="xs" shape="round"
/// variant="primary"`).
fn control_button(id: SharedString, disabled: bool, c: &buzz_ui::Colors) -> Stateful<Div> {
    let hover = c.affordance_prominent_hover;
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded_full()
        .bg(if disabled {
            c.affordance_disabled
        } else {
            c.affordance_prominent
        })
        .text_color(if disabled {
            c.text_unavailable
        } else {
            c.text_inverse
        })
        .when(!disabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(hover))
        })
}

/// The 16px upload ring: the bytes sent, or while there are no byte counts
/// (queued or preparing), a quarter arc turning every 700ms.
fn upload_ring(id: SharedString, fraction: Option<f32>, color: Hsla, reduce: bool) -> AnyElement {
    let ring = move |turn: f32| {
        let sweep = fraction.map_or(0.25, |f| f.clamp(0., 1.)) * std::f32::consts::TAU;
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                // The 24-unit artwork: radius 9, stroke 3.
                let scale = f32::from(bounds.size.width) / 24.;
                let center = bounds.center();
                let radius = 9. * scale;
                let arc = |from: f32, sweep: f32| {
                    let steps = ((sweep / 0.05).ceil() as usize).max(2);
                    let mut path = PathBuilder::stroke(px(3. * scale));
                    for step in 0..=steps {
                        let angle =
                            from + sweep * step as f32 / steps as f32 - std::f32::consts::FRAC_PI_2;
                        let at = point(
                            center.x + px(radius * angle.cos()),
                            center.y + px(radius * angle.sin()),
                        );
                        if step == 0 {
                            path.move_to(at);
                        } else {
                            path.line_to(at);
                        }
                    }
                    path.build().ok()
                };
                if let Some(track) = arc(0., std::f32::consts::TAU) {
                    window.paint_path(track, color.opacity(0.5));
                }
                if sweep > 0.
                    && let Some(progress) = arc(turn * std::f32::consts::TAU, sweep)
                {
                    window.paint_path(progress, color);
                }
            },
        )
        .size(px(16.))
    };
    if fraction.is_some() || reduce {
        return div().size(px(16.)).child(ring(0.)).into_any_element();
    }
    div()
        .size(px(16.))
        .with_animation(
            id,
            Animation::new(Duration::from_millis(700)).repeat(),
            move |d, delta| d.child(ring(delta)),
        )
        .into_any_element()
}

/// The picture a draft file shows: an image's own pixels. Other files
/// show the file icon.
fn preview_source(item: &DraftFile) -> Option<Img> {
    let kind = item
        .uploaded
        .as_ref()
        .map_or(item.mime.as_str(), |a| a.mime.as_str());
    let lower = item.name.to_ascii_lowercase();
    let image = matches!(
        kind,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) || (item.uploaded.is_none()
        && [".png", ".jpg", ".jpeg", ".gif", ".webp"]
            .iter()
            .any(|ext| lower.ends_with(ext)));
    if !image {
        return None;
    }
    Some(match &item.source {
        Source::Path(path) => img(path.clone()).object_fit(ObjectFit::Cover),
        Source::Bytes(bytes) => {
            let format = match buzz_relay_client::upload::sniff_image(bytes)? {
                "image/png" => ImageFormat::Png,
                "image/jpeg" => ImageFormat::Jpeg,
                "image/gif" => ImageFormat::Gif,
                _ => ImageFormat::Webp,
            };
            img(Arc::new(Image::from_bytes(format, bytes.to_vec()))).object_fit(ObjectFit::Cover)
        }
    })
}

/// Whether a draft is a video (`ComposerAttachments`): its type, or its
/// name before it uploads.
fn is_video(item: &DraftFile) -> bool {
    let kind = item
        .uploaded
        .as_ref()
        .map_or(item.mime.as_str(), |a| a.mime.as_str());
    let lower = item.name.to_ascii_lowercase();
    kind.starts_with("video/")
        || (item.uploaded.is_none()
            && [".mp4", ".mov", ".webm", ".m4v"]
                .iter()
                .any(|ext| lower.ends_with(ext)))
}

/// Name match tiers: exact, prefix, word prefix, substring.
fn name_match(name: &str, query: &str) -> Option<u8> {
    let query = query.trim_start().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let name = name.to_lowercase();
    if name == query.trim_end() {
        Some(0)
    } else if name.starts_with(&query) {
        Some(1)
    } else if name
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(&query))
    {
        Some(2)
    } else if name.contains(&query) {
        Some(3)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Mention,
    Channel,
    Emoji,
    /// A plugin's completion, by key (`super::composer_completions`).
    Plugin(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Query {
    trigger: Trigger,
    /// The sigil's byte offset.
    start: usize,
    /// The caret.
    end: usize,
    text: String,
}

/// The active `@`, `#` or `:` query ending at `caret` (`mentionQuery`,
/// `channelQuery`, `emojiQuery`): the sigil at the start of the text or
/// after whitespace or `([{`, at most 160 characters back. A plugin's
/// trigger in `plugins` opens its query the same way, up to the first
/// space.
fn find_query(text: &str, caret: usize, plugins: &[(char, &'static str)]) -> Option<Query> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let boundary = |start: usize| {
        before[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || "([{".contains(c))
    };
    let query = |trigger, index: usize| Query {
        trigger,
        start: index,
        end: caret,
        text: before[index + 1..].to_string(),
    };
    // What the text after the current character still allows.
    let (mut channel, mut emoji, mut plugin) = (true, true, true);
    // An `@` inside a word (an address) ends Buzz's own triggers, not a
    // plugin's query such as `/user@host`.
    let mut own = true;
    for (length, (index, c)) in before.char_indices().rev().enumerate() {
        if length > 160 || matches!(c, '\n' | '\r' | '\t') {
            return None;
        }
        // A plugin ordered before Buzz's own takes `@`, `#` or `:`.
        if "@#:".contains(c)
            && plugin
            && boundary(index)
            && caret - index <= 65
            && let Some((_, key)) = plugins.iter().find(|(trigger, _)| *trigger == c)
        {
            return Some(query(Trigger::Plugin(key), index));
        }
        match c {
            '@' if own && boundary(index) => return Some(query(Trigger::Mention, index)),
            '#' if own && channel && boundary(index) => {
                return Some(query(Trigger::Channel, index));
            }
            ':' if own && emoji && boundary(index) && (2..=64).contains(&(caret - index - 1)) => {
                return Some(query(Trigger::Emoji, index));
            }
            '@' => own = false,
            _ => {}
        }
        if plugin
            && boundary(index)
            && caret - index <= 65
            && let Some((_, key)) = plugins.iter().find(|(trigger, _)| *trigger == c)
        {
            return Some(query(Trigger::Plugin(key), index));
        }
        plugin &= !c.is_whitespace();
        channel &= !"#[](){}`<>\\".contains(c);
        emoji &= c.is_ascii_alphanumeric() || "_+-".contains(c);
    }
    None
}

struct MentionRows {
    items: Vec<Suggestion>,
    status: Option<SharedString>,
    retry: Option<channels::Retry>,
    space: Option<String>,
}

struct Completion {
    query: Query,
    items: Vec<Suggestion>,
    /// An index into the items, then the retry row.
    selected: usize,
    /// The quiet line under the list.
    status: Option<SharedString>,
    /// "Retry suggestions", after the items.
    retry: Option<channels::Retry>,
    /// The one person Space selects for this `@` query.
    space: Option<String>,
}

impl Completion {
    /// Rows the keyboard moves through.
    fn count(&self) -> usize {
        self.items.len() + usize::from(self.retry.is_some())
    }
}

/// The community people search behind `@` completion
/// (`useMentionDirectory`).
struct DirectorySearch {
    needle: String,
    attempt: u64,
    /// `None` while it waits or runs: the people and whether there are more.
    result: Option<Result<(Vec<String>, bool), String>>,
    _task: Option<Task<()>>,
}

/// Explicit picks per community and channel (`mention-history.ts`).
#[derive(Default)]
struct MentionHistory(std::collections::HashMap<String, mentions::History>);

impl Global for MentionHistory {}

/// The open-channel lookup behind `#` completion.
struct PublicSearch {
    needle: String,
    attempt: u64,
    /// `None` while it runs.
    result: Option<Result<PublicChannels, String>>,
    _task: Task<()>,
}

pub(super) enum Suggestion {
    Person {
        pubkey: String,
        /// The identity's own name, which a pick inserts.
        name: SharedString,
        /// The name shown, qualified when a namesake is present.
        label: SharedString,
        picture: Option<String>,
        agent: bool,
        member: bool,
        dm: bool,
        /// Why a row shown earlier can no longer be picked.
        disabled: Option<SharedString>,
    },
    Channel {
        id: String,
        name: SharedString,
        icon: IconName,
        /// Joined, rather than an open channel to preview.
        member: bool,
        detail: Option<SharedString>,
    },
    Emoji(Candidate),
    /// A plugin's suggestion: a pick puts `text` in place of the query.
    Text {
        label: SharedString,
        detail: Option<SharedString>,
        text: String,
        /// Why it cannot be picked.
        disabled: Option<SharedString>,
        preview: Option<SharedString>,
        /// A space typed after the query picks it.
        exact: bool,
        /// A picture before the label: the item's image, or a person's
        /// avatar as `(name, picture)`.
        leading: Option<(SharedString, Option<String>)>,
    },
    /// A saved team: its agents as `(key, name, picture)`, the detail line
    /// (its reason when disabled).
    Team {
        name: SharedString,
        recipients: Vec<(String, SharedString, Option<String>)>,
        detail: SharedString,
        disabled: bool,
        /// A space typed after a plugin's query picks it.
        exact: bool,
    },
}

/// `old`'s people in their shown order, refreshed from `new`; one that is
/// gone stays, disabled with `reason`, and one that left the conversation
/// asks to reopen. New people and the other rows follow.
fn keep_shown(
    old: &[Suggestion],
    new: Vec<Suggestion>,
    reason: impl Fn(&str) -> &'static str,
) -> Vec<Suggestion> {
    let (mut people, others): (Vec<Suggestion>, Vec<Suggestion>) = new
        .into_iter()
        .partition(|item| matches!(item, Suggestion::Person { .. }));
    let mut out = Vec::new();
    for item in old {
        let Suggestion::Person {
            pubkey,
            name,
            label,
            picture,
            agent,
            member,
            dm,
            disabled,
        } = item
        else {
            continue;
        };
        let found = people
            .iter()
            .position(|now| matches!(now, Suggestion::Person { pubkey: key, .. } if key == pubkey));
        match found.map(|at| people.remove(at)) {
            Some(Suggestion::Person {
                member: false,
                pubkey,
                name,
                label,
                picture,
                agent,
                dm,
                ..
            }) if *member => out.push(Suggestion::Person {
                pubkey,
                name,
                label,
                picture,
                agent,
                member: false,
                dm,
                disabled: Some(
                    "Channel membership changed. Reopen to review adding this recipient.".into(),
                ),
            }),
            Some(now) => out.push(now),
            None => out.push(Suggestion::Person {
                pubkey: pubkey.clone(),
                name: name.clone(),
                label: label.clone(),
                picture: picture.clone(),
                agent: *agent,
                member: *member,
                dm: *dm,
                disabled: disabled.clone().or_else(|| Some(reason(pubkey).into())),
            }),
        }
    }
    out.extend(people);
    out.extend(others);
    out
}

/// The saved teams' state line (`useTeamMentions` status).
fn teams_status(catalog: KitStatus) -> Option<&'static str> {
    match catalog {
        KitStatus::Idle | KitStatus::Loading => Some("Loading saved teams…"),
        KitStatus::Error => Some("Could not load saved teams. Retry to refresh."),
        _ => None,
    }
}

#[derive(Serialize, Deserialize)]
struct Draft {
    text: String,
    #[serde(default)]
    tokens: Vec<DraftToken>,
}

#[derive(Serialize, Deserialize)]
struct DraftToken {
    start: usize,
    end: usize,
    id: String,
    text: String,
    label: String,
}

impl From<&InputContent> for Draft {
    fn from(content: &InputContent) -> Self {
        Draft {
            text: content.text().to_string(),
            tokens: content
                .tokens()
                .iter()
                .map(|span| DraftToken {
                    start: span.range().start,
                    end: span.range().end,
                    id: span.token().id().to_string(),
                    text: span.token().text().to_string(),
                    label: span.token().label().to_string(),
                })
                .collect(),
        }
    }
}

impl Draft {
    /// The text with whichever chips still fit it.
    fn content(&self) -> InputContent {
        let mut content = InputContent::new(self.text.clone());
        for token in &self.tokens {
            let chip = InlineToken::new(token.id.clone(), token.text.clone())
                .with_label(token.label.clone());
            if let Ok(next) = content.clone().with_token(token.start..token.end, chip) {
                content = next;
            }
        }
        content
    }
}

// ---------------------------------------------------------------------------
// Rendering

impl Composer {
    fn edit_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .mt(px(-12.))
            .mx(px(-6.))
            .mb(px(-12.))
            .px(px(12.))
            .py(px(8.))
            .rounded_t(px(COMPOSER_RADIUS - 1.))
            .bg(c.surface_inset)
            .text_label_sm()
            .text_color(c.text_subtle)
            .child(Icon::new(IconName::PencilSimple).size(18.))
            .child(div().flex_1().child("Editing message"))
            .child(
                IconButton::new("cancel-edit", IconName::X)
                    .size(IconButtonSize::Sm)
                    .icon_size(18.)
                    .disabled(self.edit_status.busy)
                    .tooltip(if self.edit_status.locked {
                        "Close edit"
                    } else {
                        "Cancel edit"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx))),
            )
    }

    /// "Your edit is still in the outbox." with "Retry edit".
    fn edit_retry(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.editing.is_none() || !self.edit_status.retryable {
            return None;
        }
        let c = *cx.colors();
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .text_caption()
                .text_color(c.text_subtle)
                .child("Your edit is still in the outbox.")
                .child(
                    Button::new("retry-edit", "Retry edit")
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::RetryEdit))),
                )
                .into_any_element(),
        )
    }

    fn context_box(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = *cx.colors();
        let context = self.context.as_ref()?;
        let cancel = IconButton::new("cancel-reply", IconName::X)
            .size(IconButtonSize::Sm)
            .tooltip("Cancel reply target")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::CancelReply)));
        Some(match context {
            ReplyContext::Parent { name, excerpt } => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(12.))
                .bg(c.surface_inset)
                .text_caption()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_color(c.text_standard)
                                .child(format!("Replying to {name}")),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_color(c.text_subtle)
                                .child(excerpt.clone()),
                        ),
                )
                .child(cancel)
                .into_any_element(),
            ReplyContext::Missing => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(12.))
                .bg(c.surface_inset)
                .text_caption()
                .text_color(c.text_subtle)
                .child(div().flex_1().child("Reply target is no longer available."))
                .child(cancel)
                .into_any_element(),
        })
    }

    /// The draft's files (`ComposerAttachments.tsx`): one row that scrolls
    /// sideways. Pictures and videos show as square tiles with their
    /// control hanging off the corner; other files as 18rem chips. Every
    /// file opens in the draft viewer.
    fn attachment_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let items = self.files.read(cx).items.clone();
        let reduce = cx.reduce_motion();
        self.exiting
            .retain(|(_, _, at)| !reduce && at.elapsed() < TILE_EXIT);
        if (items.is_empty() && self.exiting.is_empty()) || self.editing.is_some() {
            return None;
        }
        if !self.exiting.is_empty()
            || items
                .iter()
                .any(|item| item.status != attachments::Status::Ready)
        {
            // The exits and the preparation ring move.
            window.request_animation_frame();
        }
        let disabled = self.sending;
        let mut children: Vec<AnyElement> = items
            .iter()
            .map(|item| match &item.voice {
                Some(voice) => self.voice_item(item, voice, disabled, cx),
                None => self.file_item(item, disabled, cx),
            })
            .collect();
        // A row that scrolls sideways does not take its tiles' height on
        // its own: the tallest tile sets it, and voice notes their card's.
        let tallest = items
            .iter()
            .chain(self.exiting.iter().map(|(_, item, _)| item))
            .filter(|item| item.voice.is_none())
            .map(|item| match item.preview() {
                attachments::Preview::Image | attachments::Preview::Video => MEDIA_TILE,
                _ => 46.,
            })
            .reduce(f32::max);
        for (index, item, at) in self.exiting.clone() {
            let tile = self.exiting_item(&item, at, cx);
            children.insert(index.min(children.len()), tile);
        }
        // Scroll padding keeps the hanging controls inside the scroller.
        // buzz-app cancels it with negative margins, which take a GPUI
        // row's height away, so the tiles sit that much lower here.
        Some(
            div()
                .id("composer-attachments")
                .flex()
                .flex_none()
                .when_some(tallest, |d, tallest| d.h(px(tallest + 10.)))
                .gap(px(12.))
                .min_w_0()
                .pt(px(10.))
                .pr(px(10.))
                .overflow_x_scroll()
                .children(children)
                .into_any_element(),
        )
    }

    /// A removed file shrinking away (scale 0.75, 12px down, faded, 180ms).
    fn exiting_item(&self, item: &DraftFile, at: Instant, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let t = ease_enter((at.elapsed().as_secs_f32() / TILE_EXIT.as_secs_f32()).min(1.));
        let scale = 1. - 0.25 * t;
        let media = matches!(
            item.preview(),
            attachments::Preview::Image | attachments::Preview::Video
        );
        let (width, height) = if media {
            (MEDIA_TILE, MEDIA_TILE)
        } else {
            (288., 46.)
        };
        let content = if media {
            preview_source(item)
                .or_else(|| self.poster(item, cx))
                .map(|source| source.size_full().into_any_element())
        } else {
            None
        };
        div()
            .flex_none()
            .w(px(width))
            .h(px(height))
            .flex()
            .items_center()
            .justify_center()
            .opacity(1. - t)
            .child(
                div()
                    .relative()
                    .top(px(12. * t))
                    .w(px(width * scale))
                    .h(px(height * scale))
                    .overflow_hidden()
                    .rounded(px(if media { MEDIA_RADIUS } else { 8. }))
                    .border_1()
                    .border_color(c.border_standard)
                    .bg(c.surface_panel)
                    .children(content),
            )
            .into_any_element()
    }

    fn file_item(&self, item: &DraftFile, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let id = SharedString::from(item.id.clone());
        let size = attachments::file_size(item.uploaded.as_ref().map_or(item.size, |a| a.size));
        let status = item.status.label();
        let media = matches!(
            item.preview(),
            attachments::Preview::Image | attachments::Preview::Video
        );
        let source = if media {
            preview_source(item).or_else(|| self.poster(item, cx))
        } else {
            None
        };
        let error = item.status == attachments::Status::Error;
        let busy = item.status == attachments::Status::Queued || item.status.busy();
        let group = SharedString::from(format!("attachment-{}", item.id));
        let reduce = cx.reduce_motion();
        // The corner control: progress (Cancel on hover), Retry, or Remove.
        let control_id = id.clone();
        let control =
            control_button(
                SharedString::from(format!("attachment-control-{id}")),
                disabled,
                &c,
            )
            .map(|d| {
                if busy {
                    let fraction = item
                        .transfer
                        .map(|(sent, total)| sent as f32 / total.max(1) as f32);
                    d.tooltip(Tooltip::text("Cancel upload and remove attachment"))
                        .child(div().group_hover(group.clone(), |s| s.opacity(0.)).child(
                            upload_ring(
                                SharedString::from(format!("upload-ring-{id}")),
                                fraction,
                                c.text_inverse,
                                reduce,
                            ),
                        ))
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .opacity(0.)
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .child(Icon::new(IconName::X).size(16.)),
                        )
                } else if error {
                    d.tooltip(Tooltip::text(format!(
                        "{} Press Delete to remove.",
                        item.error.clone().unwrap_or_else(|| "Retry upload".into())
                    )))
                    .child(Icon::new(IconName::ArrowsClockwise).size(16.))
                } else {
                    d.child(Icon::new(IconName::X).size(16.))
                }
            })
            .when(!disabled, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| {
                    if error {
                        this.retry_attachment(&control_id, cx)
                    } else {
                        this.remove_attachment(&control_id, cx)
                    }
                }))
            });
        let remove_id = id.clone();
        let remove = error.then(|| {
            control_button(
                SharedString::from(format!("attachment-remove-{id}")),
                disabled,
                &c,
            )
            .tooltip(Tooltip::text(format!("Remove {}", item.name)))
            .child(Icon::new(IconName::X).size(16.))
            .when(!disabled, |d| {
                d.on_click(
                    cx.listener(move |this, _, _, cx| this.remove_attachment(&remove_id, cx)),
                )
            })
        });
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .self_start()
            .child(control)
            .children(remove);
        let open_id = id.clone();
        let open = cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.open_preview(&open_id, window, cx)
        });
        if media {
            let hidden = media::draft_tile_hidden(&item.id, cx);
            let preview = div()
                .id(SharedString::from(format!("attachment-preview-{id}")))
                .relative()
                .size_full()
                .rounded(px(MEDIA_RADIUS))
                .overflow_hidden()
                .border_1()
                .border_color(c.border_standard)
                .bg(c.surface_panel)
                .cursor_pointer()
                .child(media::track_tile(&media::draft_tile_key(&item.id)))
                .map(|d| match source {
                    Some(source) => d.child(source.size_full().when(hidden, |s| s.invisible())),
                    None => d
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::FileText).size(24.).color(c.text_subtle)),
                })
                .on_click(open);
            // Only Remove waits for hover; progress and Retry stay shown.
            let quiet = !busy && !error;
            return div()
                .id(SharedString::from(format!("attachment-{id}")))
                .group(group)
                .relative()
                .flex_none()
                .size(px(MEDIA_TILE))
                .child(preview)
                .child(
                    div()
                        .absolute()
                        .top(px(-10.))
                        // The last control centres on the corner, even after Retry.
                        .right(px(-10.))
                        .rounded_full()
                        .bg(c.surface_panel)
                        .when(quiet, |d| {
                            d.opacity(0.).group_hover(
                                SharedString::from(format!("attachment-{}", item.id)),
                                |s| s.opacity(1.),
                            )
                        })
                        .child(actions),
                )
                .into_any_element();
        }
        let thumb = div()
            .id(SharedString::from(format!("attachment-preview-{id}")))
            .relative()
            .flex_none()
            .size(px(40.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.))
            .overflow_hidden()
            .bg(c.surface_panel)
            .text_color(c.text_subtle)
            .cursor_pointer()
            .child(Icon::new(IconName::FileText).size(24.))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(c.border_standard),
            )
            .on_click(open);
        div()
            .id(SharedString::from(format!("attachment-{id}")))
            .group(group)
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .w(px(288.))
            .max_w_full()
            .h(px(46.))
            .p(px(2.))
            .min_w_0()
            .rounded(px(8.))
            .border_1()
            .border_color(c.border_standard)
            .child(thumb)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_label_sm()
                            .text_color(c.text_standard)
                            .truncate()
                            .child(item.name.clone()),
                    )
                    .child(
                        div()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .truncate()
                            .child(format!("{size} · {status}")),
                    ),
            )
            .child(actions)
            .into_any_element()
    }

    /// The community's background sends (`BackgroundUploadStatus.tsx`):
    /// the phase, a 64px bar of the bytes sent (an indeterminate sweep
    /// before there are byte counts) and Cancel, which stops the newest
    /// send and hands it back.
    fn background_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let queue = self.queue.read(cx);
        if !queue.uploading() {
            return None;
        }
        let progress = queue.progress(cx);
        let c = *cx.colors();
        let reduce = cx.reduce_motion();
        let bar = div()
            .relative()
            .flex_none()
            .w(px(64.))
            .h(px(4.))
            .rounded_full()
            .overflow_hidden()
            .bg(c.surface_inset)
            .child(match progress.fraction {
                Some(fraction) => div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(64. * fraction.clamp(0., 1.) as f32))
                    .rounded_full()
                    .bg(c.affordance_prominent)
                    .into_any_element(),
                None if reduce => div()
                    .absolute()
                    .inset_0()
                    .rounded_full()
                    .bg(c.affordance_prominent)
                    .opacity(0.4)
                    .into_any_element(),
                None => div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(64. / 3.))
                    .rounded_full()
                    .bg(c.affordance_prominent)
                    .with_animation(
                        "background-upload-sweep",
                        Animation::new(Duration::from_millis(1200))
                            .repeat()
                            .with_easing(ease_in_out),
                        |sweep, delta| {
                            // translateX(-100%) to translateX(300%).
                            sweep.left(px(64. / 3. * (4. * delta - 1.)))
                        },
                    )
                    .into_any_element(),
            });
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_caption()
                .text_color(c.text_subtle)
                .child(progress.phase)
                .child(bar)
                .child(
                    Button::new("cancel-background-upload", "Cancel")
                        .variant(ButtonVariant::Ghost)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.queue.update(cx, |queue, cx| queue.cancel_newest(cx));
                        })),
                )
                .into_any_element(),
        )
    }

    /// "Could not attach file", and the queue's notices when this composer
    /// hosts them: toasts in the toast region.
    fn toasts(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let host = cx.entity_id();
        let queue = self.queue.read(cx);
        let notices = if queue.hosts_notices(host) {
            queue.notices.clone()
        } else {
            Vec::new()
        };
        if notices.is_empty() && self.attach_error.is_none() {
            return None;
        }
        let mut cards: Vec<AnyElement> = Vec::new();
        if let Some(error) = &self.attach_error {
            cards.push(
                Toast::new("attach-error", "Could not attach file")
                    .tone(ToastTone::Error)
                    .description(error.clone())
                    .on_dismiss(cx.listener(|this, _, _, cx| {
                        this.attach_error = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        for notice in notices {
            let id = notice.id.clone();
            let toast = Toast::new(
                SharedString::from(format!("upload-notice-{}", notice.id)),
                notice.message.clone(),
            )
            .tone(ToastTone::Error);
            let toast = match notice.recovery {
                Some(job) => toast.action(
                    Button::new(
                        SharedString::from(format!("retry-recovery-{job}")),
                        "Retry failed send recovery",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.queue
                            .update(cx, |queue, cx| queue.retry_recovery(&job, cx));
                    })),
                ),
                None => toast.on_dismiss(cx.listener(move |this, _, _, cx| {
                    this.queue.update(cx, |queue, cx| queue.dismiss(&id, cx));
                })),
            };
            cards.push(toast.into_any_element());
        }
        super::toasts::stack(window, cards)
    }

    /// The saved draft's trouble and what to do about it
    /// (`MessageComposer.tsx`'s recovery notices).
    fn draft_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let issue = self.draft_issue.filter(|_| self.editing.is_none())?;
        let c = *cx.colors();
        let message = match issue {
            DraftIssue::Accepted => {
                "Message accepted. Could not update the saved draft. Retry cleanup before sending again."
            }
            DraftIssue::Conflict => {
                "This draft changed elsewhere. Your edits are kept here; choose which draft to keep."
            }
            DraftIssue::Storage => {
                "Could not save this draft on this device. Your edits are kept here."
            }
        };
        let buttons: Vec<AnyElement> = match issue {
            DraftIssue::Accepted => vec![
                Button::new("retry-draft-cleanup", "Retry draft cleanup")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.persist(true, cx) {
                            this.draft_issue = None;
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
            DraftIssue::Conflict => vec![
                Button::new("load-saved-draft", "Load saved draft")
                    .on_click(cx.listener(|this, _, window, cx| this.load_saved_draft(window, cx)))
                    .into_any_element(),
                Button::new("keep-my-draft", "Keep my draft")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.persist(true, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
            DraftIssue::Storage => vec![
                Button::new("retry-draft-save", "Retry draft save")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.persist(false, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .px(px(8.))
                .text_body()
                .text_color(c.text_standard)
                .child(message)
                .child(div().flex().flex_wrap().gap(px(8.)).children(buttons))
                .into_any_element(),
        )
    }

    /// "Retry message preparation": a send failed while the custom emoji
    /// catalog is in error, which preparing a message needs.
    fn preparation_retry(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.error.as_ref()?;
        let community = self.community.read(cx);
        if !matches!(
            community.snapshot.emoji.status,
            buzz_relay_client::CatalogStatus::Error(_)
        ) {
            return None;
        }
        Some(
            div()
                .px(px(8.))
                .child(
                    Button::new("retry-preparation", "Retry message preparation")
                        .disabled(self.input_disabled(cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.community.read(cx).session.refresh_emoji();
                            this.retrying_preparation = true;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// A video draft's first frame, decoded once in the background
    /// (`<video preload="metadata">` at 0.1s).
    fn poster(&self, item: &DraftFile, cx: &mut Context<Self>) -> Option<Img> {
        if !is_video(item) {
            return None;
        }
        let Source::Path(path) = &item.source else {
            return None;
        };
        if let Some(poster) = self.posters.borrow().get(&item.id) {
            return poster
                .clone()
                .map(|image| img(image).object_fit(ObjectFit::Cover));
        }
        self.posters.borrow_mut().insert(item.id.clone(), None);
        let (id, path) = (item.id.clone(), path.clone());
        let probe = cx.background_executor().spawn(async move {
            media::probe_file(&path)
                .ok()
                .and_then(|probe| probe.frame)
                .and_then(|frame| media::render_image(frame.width, frame.height, frame.bgra))
        });
        cx.spawn(async move |this, cx| {
            let poster = probe.await;
            let _ = this.update(cx, |this, cx| {
                this.posters.borrow_mut().insert(id, poster);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Opens a draft file in the fullscreen draft viewer.
    fn open_preview(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        media::open_draft_preview(self.files.clone(), id.to_string(), window, cx);
    }

    fn tools(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        if self.formatting {
            let active = self.active_formats(cx);
            let state = self.input.read(cx);
            let (text, selection) = (state.value().to_string(), state.selected_range());
            let button = |format: Format, cx: &mut Context<Self>| {
                IconButton::new(
                    SharedString::from(format!("format-{format:?}")),
                    format.icon(),
                )
                .size(IconButtonSize::Sm)
                .selected(match format {
                    Format::Bold => active.bold,
                    Format::Italic => active.italic,
                    Format::Strike => active.strike,
                    Format::Code => active.code,
                    Format::Spoiler => active.spoiler,
                    Format::Link => false,
                    _ => format::active(&text, selection.clone(), format),
                })
                .tooltip(format.tooltip())
                .on_click(cx.listener(move |this, _, window, cx| this.format(format, window, cx)))
            };
            let instant = self.toggle_instant(cx);
            return div()
                .id("format-tools")
                .flex()
                .flex_1()
                .items_center()
                .min_w_0()
                .child(
                    IconButton::new("close-format", IconName::X)
                        .size(IconButtonSize::Sm)
                        .variant(ButtonVariant::Subtle)
                        .tooltip("Close formatting")
                        .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                            this.toggle_formatting(false, event.is_keyboard(), window, cx);
                        })),
                )
                .child(
                    div()
                        .w(px(1.))
                        .h(px(20.))
                        .mx(px(4.))
                        .bg(c.border_standard)
                        .opacity(0.6),
                )
                .child(
                    div()
                        .id("format-buttons")
                        .flex()
                        .gap(px(2.))
                        .overflow_x_scroll()
                        .children(Format::ALL.into_iter().map(|format| button(format, cx))),
                )
                .with_animation(
                    ("format-reveal", self.format_toggles),
                    reveal(instant),
                    |tools, delta| tools.opacity(delta),
                )
                .into_any_element();
        }
        let instant = self.toggle_instant(cx);
        let editing = self.editing.is_some();
        div()
            .flex()
            .flex_1()
            .items_center()
            .gap(px(4.))
            .min_h(px(36.))
            .child(self.leading_tools(cx))
            // Disabled, not hidden, while editing (`renderLeadingTools`).
            .when(!self.no_attachments, |d| {
                d.child(
                    IconButton::new("attach", IconName::Paperclip)
                        .size(IconButtonSize::Sm)
                        .tooltip("Attach files")
                        .disabled(editing || self.sending)
                        .on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx))),
                )
            })
            .children(self.tools_in(super::composer_tools::Slot::BeforeEmoji))
            .child(
                div()
                    .relative() // The emoji picker is the Emoji plugin's.
                    .when(crate::plugins::enabled("buzz.emoji"), |toolbar| {
                        toolbar.child(
                            IconButton::new("emoji", IconName::Smiley)
                                .size(IconButtonSize::Sm)
                                .selected(self.picker_open)
                                .tooltip("Emoji and GIFS")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    // The press outside that just closed it.
                                    if this
                                        .picker_closed
                                        .take()
                                        .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
                                    {
                                        return;
                                    }
                                    this.picker_open = !this.picker_open;
                                    if this.picker_open {
                                        this.picker.update(cx, |picker, cx| {
                                            picker.reset(window, cx);
                                            picker.focus(window, cx);
                                        });
                                    } else {
                                        this.focus(window, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .children(self.tools_in(super::composer_tools::Slot::BeforeVoice))
            // A plain 32px button: no hover fill, an 18px glyph. While
            // editing it stays but records nothing (`useVoiceCapture`).
            // The recorder is the Voice Notes plugin's (`voice-notes`).
            .when(crate::plugins::enabled("buzz.voice-notes"), |toolbar| {
                toolbar.child(
                    div()
                        .id("record-voice-note")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(32.))
                        .rounded(px(10.))
                        .text_color(if self.sending {
                            c.text_unavailable
                        } else {
                            c.text_subtle
                        })
                        .when(!self.sending, |d| d.cursor_pointer())
                        .tooltip(Tooltip::text("Record voice note"))
                        .child(Icon::new(IconName::Microphone).size(18.))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_capture(window, cx)),
                        ),
                )
            })
            .children(self.tools_in(super::composer_tools::Slot::AfterVoice))
            .child(
                div()
                    .relative()
                    .child(
                        IconButton::new("format", IconName::TextAa)
                            .size(IconButtonSize::Sm)
                            .tooltip("Formatting")
                            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                this.toggle_formatting(true, event.is_keyboard(), window, cx);
                            })),
                    )
                    .with_animation(
                        ("format-toggle-reveal", self.format_toggles),
                        reveal(instant),
                        |toggle, delta| toggle.opacity(delta).left(px(-8. * (1. - delta))),
                    ),
            )
            // The leading tools slide in as the toolbar closes.
            .relative()
            .with_animation(
                ("tools-reveal", self.format_toggles),
                reveal(instant),
                |tools, delta| tools.opacity(delta).left(px(-12. * (1. - delta))),
            )
            .into_any_element()
    }

    /// Opens or closes the formatting toolbar. Keyboard toggles and reduced
    /// motion skip the 140ms reveal (`ComposerFormattingTools.tsx`).
    fn toggle_formatting(
        &mut self,
        open: bool,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.formatting = open;
        self.format_toggles += 1;
        self.format_keyboard = keyboard;
        self.focus(window, cx);
        cx.notify();
    }

    fn toggle_instant(&self, cx: &App) -> bool {
        self.format_toggles == 0 || self.format_keyboard || cx.reduce_motion()
    }

    /// `.composerLeadingTools`: the mention button, and once chips name
    /// recipients, a pill of their avatars that remove them on click.
    /// The plugin tools in one toolbar slot, in order.
    fn tools_in(&self, slot: super::composer_tools::Slot) -> Vec<AnyView> {
        self.tools
            .iter()
            .filter(|(at, _)| *at == slot)
            .map(|(_, (_, _, tool))| tool.clone())
            .collect()
    }

    fn leading_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let recipients = self.recipients(cx);
        let community = self.community.read(cx);
        let media = community.session.media().clone();
        let disabled = self.sending;
        let reduce = cx.reduce_motion();
        let buttons = recipients
            .iter()
            .enumerate()
            .map(|(index, (pubkey, name))| {
                let profile = community.snapshot.profiles.get(pubkey);
                let agent = profile.is_some_and(|p| p.is_agent);
                let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Small);
                if agent {
                    avatar = avatar.squircle();
                }
                if let Some(picture) = profile.and_then(|p| p.picture.as_deref()) {
                    avatar = avatar.image(media::avatar(picture, &media));
                }
                let group = SharedString::from(format!("mention-recipient-{index}"));
                let key = pubkey.clone();
                div()
                    .id(("mention-recipient", index))
                    .group(group.clone())
                    .absolute()
                    .top_0()
                    .left(px(index as f32 * 22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .border_2()
                    .border_color(c.affordance_subtle)
                    .bg(c.affordance_subtle)
                    .map(|d| {
                        if agent {
                            d.rounded(px(9.))
                        } else {
                            d.rounded_full()
                        }
                    })
                    .overflow_hidden()
                    .tooltip(Tooltip::text(format!(
                        "Remove explicit mention of {name} ({})",
                        &pubkey[..8.min(pubkey.len())]
                    )))
                    .child(avatar)
                    .when(!disabled, |d| {
                        d.cursor_pointer()
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .map(|d| {
                                        if agent {
                                            d.rounded(px(7.))
                                        } else {
                                            d.rounded_full()
                                        }
                                    })
                                    .bg(c.affordance_prominent)
                                    .text_color(c.text_inverse)
                                    .opacity(0.)
                                    .group_hover(group, |s| s.opacity(1.))
                                    .child(Icon::new(IconName::X).size(16.)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_recipient(&key, window, cx)
                            }))
                    })
                    // A new recipient's avatar springs in from 0.9 scale
                    // (240ms, bounce 0.15), about its centre.
                    .with_animation(
                        SharedString::from(format!("recipient-in-{pubkey}")),
                        Animation::new(Duration::from_millis(if reduce { 1 } else { 240 }))
                            .with_easing(spring),
                        move |button, t| {
                            let size = 28. * (0.9 + 0.1 * t);
                            let inset = (28. - size) / 2.;
                            button
                                .size(px(size))
                                .top(px(inset))
                                .left(px(index as f32 * 22. + inset))
                        },
                    )
            });
        let has_recipients = !recipients.is_empty();
        let count = recipients.len();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .rounded(px(12.))
            .when(has_recipients, |d| {
                d.rounded_full().px(px(2.)).bg(c.affordance_subtle)
            })
            // Plugin tools ordered below 0 lead (`renderLeading`).
            .children(self.tools_in(super::composer_tools::Slot::BeforeMention))
            // The mention picker is the Mentions plugin's.
            .when(crate::plugins::enabled("buzz.mentions"), |toolbar| {
                toolbar.child(
                    IconButton::new("mention", IconName::At)
                        .size(IconButtonSize::Sm)
                        .tooltip("Mention a member")
                        .selected(self.mention_picker.is_some())
                        .disabled(self.input_disabled(cx))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_mention_picker(window, cx)
                        })),
                )
            })
            .children(self.tools_in(super::composer_tools::Slot::AfterMention))
            .when(has_recipients, |d| {
                d.child(
                    // 28px buttons overlapping by 6px, then 6px of pill.
                    div()
                        .id("mention-recipients")
                        .relative()
                        .flex_none()
                        .w(px(count as f32 * 22. + 12.))
                        .h(px(28.))
                        .children(buttons),
                )
            })
    }

    /// A recorded note waiting to send: the voice card, 21rem wide, with
    /// its remove button on the corner (`ComposerAttachments.tsx:128-170`).
    fn voice_item(
        &self,
        item: &DraftFile,
        voice: &Voice,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let id = SharedString::from(item.id.clone());
        let media = self.community.read(cx).session.media().clone();
        // Once uploaded, renderers see the hosted file (`uploaded?.url`).
        let (url, mime) = match &item.uploaded {
            Some(uploaded) => (uploaded.url.clone(), uploaded.mime.clone()),
            None => (voice.key.clone(), item.mime.clone()),
        };
        let attachment = buzz_relay_client::fold::Attachment {
            url: url.clone(),
            kind: buzz_relay_client::fold::AttachmentKind::Audio,
            mime: Some(mime),
            size: Some(item.size),
            name: Some(item.name.clone()),
            duration: Some(voice.duration),
            waveform: Some(voice.waveform.clone()),
            voice_note: true,
            dimensions: None,
            blurhash: None,
            preview_url: None,
            insecure: false,
        };
        let busy = item.status.busy();
        let (remove_id, retry_id) = (id.clone(), id.clone());
        if !disabled {
            let (composer, id) = (cx.entity().downgrade(), id.clone());
            super::renderers::removable(
                &url,
                std::rc::Rc::new(move |cx: &mut App| {
                    let _ = composer.update(cx, |this, cx| this.remove_attachment(&id, cx));
                }),
            );
        } else {
            // Sending: plugins may no longer remove it (`onRemove` goes).
            super::renderers::not_removable(&url);
        }
        div()
            .id(SharedString::from(format!("voice-draft-{id}")))
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .w(px(336.))
            .max_w_full()
            .min_w_0()
            // A plugin's attachment renderer draws drafts too
            // (`ComposerAttachments`); Buzz keeps the controls around it.
            .child(
                super::renderers::attachment(
                    &attachment,
                    SharedString::from(format!("voice-draft-plugin-{id}")),
                    &media,
                    cx,
                )
                .unwrap_or_else(|| {
                    media::voice_draft(
                        SharedString::from(format!("voice-draft-card-{id}")),
                        &attachment,
                        &media,
                        cx,
                    )
                }),
            )
            .when_some(item.error.clone(), |d, error| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(4.))
                        .text_caption()
                        .text_color(c.text_danger)
                        .child(error)
                        .child(
                            Button::new(
                                SharedString::from(format!("retry-voice-{id}")),
                                "Retry voice note upload",
                            )
                            .disabled(disabled)
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.retry_attachment(&retry_id, cx),
                            )),
                        ),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("remove-voice-{id}")))
                    .absolute()
                    .top(px(-1.))
                    .right(px(-5.))
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(c.border_standard)
                    .bg(c.surface_popover)
                    .text_color(c.text_standard)
                    .when(!disabled, |d| d.cursor_pointer())
                    .tooltip(Tooltip::text("Remove voice note"))
                    .map(|d| {
                        if busy {
                            d.child(Spinner::new().size(12.))
                        } else {
                            d.child(Icon::new(IconName::X).size(16.))
                        }
                    })
                    .when(!disabled, |d| {
                        d.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.remove_attachment(&remove_id, cx)
                        }))
                    }),
            )
            .into_any_element()
    }

    /// The recorder in place of the tools (`VoiceCapture.tsx`): discard,
    /// a divider, the timer, the live waveform and the finish button.
    fn capture_row(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let capture = self.capture.as_ref()?;
        let c = *cx.colors();
        let failed = matches!(capture, Capture::Failed(_));
        // The row holds focus for its one focused button: Discard while
        // recording, Close after a failure. Enter or Space presses it.
        let focused = self.capture_focus.is_focused(window);
        let ring = move |button: AnyElement, on: bool| {
            div()
                .flex_none()
                .rounded(px(10.))
                .border_2()
                .border_color(if on {
                    c.border_focus
                } else {
                    gpui_kit::transparent_black()
                })
                .child(button)
        };
        let discard = IconButton::new("discard-voice-note", IconName::X)
            .size(IconButtonSize::Sm)
            .icon_size(18.)
            .tooltip("Discard voice note")
            .disabled(matches!(capture, Capture::Preparing))
            .on_click(cx.listener(|this, _, window, cx| this.discard_capture(window, cx)));
        let row = div()
            .id("voice-capture")
            .track_focus(&self.capture_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "escape" | "enter" | "space") {
                    cx.stop_propagation();
                    this.discard_capture(window, cx);
                }
            }))
            .flex()
            .items_center()
            .gap(px(4.))
            .min_h(px(40.))
            .child(ring(discard.into_any_element(), focused && !failed))
            .child(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(px(20.))
                    .mx(px(4.))
                    .bg(c.border_standard),
            );
        if let Capture::Plugin(view) = capture {
            return Some(
                row.child(div().flex_1().min_w_0().child(view.clone()))
                    .into_any_element(),
            );
        }
        if let Capture::Failed(error) = capture {
            return Some(
                row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_caption()
                        .text_color(c.text_danger)
                        .child(error.clone()),
                )
                .child(ring(
                    Button::new("close-voice-note", "Close")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.discard_capture(window, cx)),
                        )
                        .into_any_element(),
                    focused,
                ))
                .into_any_element(),
            );
        }
        let recorder = match capture {
            Capture::Recording(recorder) => Some(recorder),
            _ => None,
        };
        let live = recorder.is_some_and(Recorder::live);
        let timer = match recorder {
            Some(recorder) if live => div().flex().child(voice_time(recorder.seconds())).child(
                div()
                    .text_color(c.text_subtle)
                    .child(format!(" / {}", voice_time(recorder::MAX_SECONDS))),
            ),
            Some(_) => div().child("Waiting for microphone…"),
            None => div().child("Preparing voice note…"),
        };
        let levels = recorder.map(|r| r.levels(256)).unwrap_or_default();
        // Each new level slides the strip in from 5px over one interval
        // (`VoiceNoteRecorder`), unless motion is reduced.
        let interval = Duration::from_millis(120);
        let slide = match recorder {
            Some(recorder) if live && !cx.reduce_motion() => {
                let count = recorder.level_count();
                let (seen, _) = self.wave_tick.get();
                let now = std::time::Instant::now();
                if count != seen {
                    let advanced = count == seen + 1;
                    self.wave_tick
                        .set((count, if advanced { now } else { now - interval }));
                }
                let elapsed = now.duration_since(self.wave_tick.get().1);
                let left = 1. - (elapsed.as_secs_f32() / interval.as_secs_f32()).min(1.);
                if left > 0. {
                    window.request_animation_frame();
                }
                5. * left
            }
            _ => 0.,
        };
        Some(
            row.child(
                timer
                    .flex_none()
                    .text_caption()
                    .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
                    .text_color(c.text_standard),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h(px(24.))
                    .mx(px(8.))
                    .items_center()
                    .justify_end()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(2.))
                            .relative()
                            .left(px(slide))
                            .children(levels.into_iter().map(move |level| {
                                div()
                                    .flex_none()
                                    .w(px(3.))
                                    .h(px(recorder::bar_height(level)))
                                    .rounded_full()
                                    .bg(c.text_standard)
                            })),
                    ),
            )
            .child(
                IconButton::new("finish-voice-note", IconName::StopFilled)
                    .size(IconButtonSize::Sm)
                    .icon_size(16.)
                    .variant(ButtonVariant::Prominent)
                    .shape(IconButtonShape::Round)
                    .tooltip("Finish voice note")
                    .disabled(!live)
                    .on_click(cx.listener(|this, _, window, cx| this.finish_capture(window, cx))),
            )
            .into_any_element(),
        )
    }

    /// "Mention people outside this channel?" (`useNonmemberMentions.tsx`).
    fn outsiders_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.outsiders.as_ref()?;
        let c = *cx.colors();
        let profiles = &self.community.read(cx).snapshot.profiles;
        let names = prompt
            .missing
            .iter()
            .map(|key| message::display_name(profiles, key).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let verb = if prompt.missing.len() == 1 {
            "is"
        } else {
            "are"
        };
        let next = if prompt.can_add {
            "Invite them to the channel, or send without inviting them."
        } else {
            "You cannot add people to this channel. You can still send without inviting them."
        };
        let busy = prompt.busy;
        let dialog = Dialog::new("outsider-mentions", "Mention people outside this channel?")
            .description(format!("{names} {verb} not in this channel. {next}"))
            .on_close(cx.listener(|this, _, window, cx| this.close_outsiders(window, cx)))
            .children(
                prompt
                    .error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .footer(
                Button::new(
                    "outsiders-skip",
                    if prompt.can_add {
                        "Do nothing"
                    } else {
                        "Send anyway"
                    },
                )
                .surface(Surface::Floating)
                .disabled(busy)
                .on_click(
                    cx.listener(|this, _, window, cx| this.send_without_outsiders(window, cx)),
                ),
            )
            .when(prompt.can_add, |d| {
                d.footer(
                    Button::new(
                        "outsiders-invite",
                        if busy { "Inviting…" } else { "Invite" },
                    )
                    .prominent()
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.invite_outsiders(window, cx))),
                )
            });
        let content = div()
            .track_focus(&self.outsiders_focus)
            .key_context("OutsiderMentions")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.close_outsiders(window, cx),
                    // The default button has the focus: Invite when the
                    // viewer can add people, else "Send anyway".
                    "enter" => match this.outsiders.as_ref().is_some_and(|p| p.can_add) {
                        true => this.invite_outsiders(window, cx),
                        false => this.send_without_outsiders(window, cx),
                    },
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(dialog);
        Some(overlay::modal(window, content, None).into_any_element())
    }

    fn completion_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let completion = self.completion.as_ref()?;
        let c = *cx.colors();
        let trigger = completion.query.trigger;
        let compact = trigger == Trigger::Emoji;
        let media = self.community.read(cx).session.media().clone();
        let mut shortcodes: std::collections::HashMap<&str, usize> = Default::default();
        for item in &completion.items {
            if let Suggestion::Emoji(candidate) = item {
                *shortcodes.entry(candidate.shortcode()).or_default() += 1;
            }
        }
        let rows = completion.items.iter().enumerate().map(|(index, item)| {
            let selected = index == completion.selected;
            let row = div()
                .id(("completion", index))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(if compact { 9. } else { 3. }))
                .cursor_pointer()
                .when(selected, |d| d.bg(c.affordance_popover_selected))
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(completion) = &mut this.completion
                        && completion.selected != index
                    {
                        completion.selected = index;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| this.accept(index, window, cx)));
            match item {
                Suggestion::Person {
                    pubkey,
                    label: name,
                    picture,
                    agent,
                    member,
                    dm,
                    disabled,
                    ..
                } => {
                    let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Default);
                    if *agent {
                        avatar = avatar.squircle();
                    }
                    if let Some(picture) = picture {
                        avatar = avatar.image(media::avatar(picture, &media));
                    }
                    let detail = if let Some(reason) = disabled {
                        div().text_color(c.text_subtle).child(reason.clone())
                    } else if *member {
                        div()
                            .font_family(buzz_ui::tokens::font::MONO)
                            .text_color(c.text_metadata)
                            .max_w(px(96.))
                            .overflow_hidden()
                            .child(pubkey[..12].to_string())
                    } else {
                        div().text_color(c.text_subtle).child(if *dm {
                            "Not in DM · Will not be notified"
                        } else {
                            "Not in channel · Choose whether to add when you send"
                        })
                    };
                    row.h(px(48.))
                        .p(px(8.))
                        .when(disabled.is_some(), |d| {
                            d.cursor_default().text_color(c.text_subtle)
                        })
                        .child(avatar)
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .max_w(px(200.))
                                .truncate()
                                .text_caption()
                                .child(detail),
                        )
                        .into_any_element()
                }
                Suggestion::Team {
                    name,
                    recipients,
                    detail,
                    disabled,
                    ..
                } => {
                    // Up to four agents, then "+N" (`TeamMentionAvatars`).
                    let avatars = div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(4.))
                        .children(recipients.iter().take(4).map(|(_, name, picture)| {
                            let mut avatar =
                                Avatar::new(name.clone()).size(AvatarSize::Small).squircle();
                            if let Some(picture) = picture {
                                avatar = avatar.image(media::avatar(picture, &media));
                            }
                            avatar
                        }))
                        .when(recipients.len() > 4, |d| {
                            d.child(
                                div()
                                    .text_caption()
                                    .text_color(c.text_subtle)
                                    .child(format!("+{}", recipients.len() - 4)),
                            )
                        });
                    row.h(px(48.))
                        .p(px(8.))
                        .when(*disabled, |d| d.cursor_default().text_color(c.text_subtle))
                        .child(avatars)
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .max_w(px(200.))
                                .truncate()
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child(detail.clone()),
                        )
                        .into_any_element()
                }
                Suggestion::Text {
                    label,
                    detail,
                    disabled,
                    preview,
                    leading,
                    ..
                } => row
                    .min_h(px(40.))
                    .px(px(8.))
                    .when(disabled.is_some(), |row| row.opacity(0.5))
                    .children(leading.as_ref().map(|(name, picture)| {
                        let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Small);
                        let image = match picture {
                            Some(data) if data.starts_with("data:") => media::inline_image(data),
                            Some(url) => Some(media::avatar(url, &media)),
                            None => None,
                        };
                        if let Some(image) = image {
                            avatar = avatar.image(image);
                        }
                        avatar
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(label.clone()))
                            .children(preview.clone().map(|preview| {
                                div()
                                    .truncate()
                                    .text_caption()
                                    .text_color(c.text_subtle)
                                    .child(preview)
                            })),
                    )
                    .children(disabled.clone().or_else(|| detail.clone()).map(|detail| {
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .max_w(px(200.))
                            .truncate()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .child(detail)
                    }))
                    .into_any_element(),
                Suggestion::Channel {
                    name, icon, detail, ..
                } => row
                    .h(px(48.))
                    .p(px(8.))
                    .child(
                        div()
                            .size(px(32.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(*icon).size(22.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(name.clone()))
                            .children(detail.clone().map(|detail| {
                                div()
                                    .text_caption()
                                    .text_color(c.text_subtle)
                                    .truncate()
                                    .child(detail)
                            })),
                    )
                    .when(selected, |d| {
                        d.child(
                            div()
                                .px(px(8.))
                                .py(px(4.))
                                .rounded(px(4.))
                                .bg(c.surface_inset)
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child("Enter"),
                        )
                    })
                    .into_any_element(),
                Suggestion::Emoji(candidate) => {
                    // A shared shortcode says which catalog each row is from.
                    let detail = (shortcodes.get(candidate.shortcode()).copied().unwrap_or(0) > 1)
                        .then_some(match candidate {
                            Candidate::Custom(_) => "Community emoji",
                            Candidate::Unicode(_) => "Unicode emoji",
                        });
                    let preview = match candidate {
                        Candidate::Unicode(emoji) => div()
                            .text_size(px(24.))
                            .line_height(px(28.))
                            .child(emoji.as_str())
                            .into_any_element(),
                        Candidate::Custom(emoji) => img(media::image(&emoji.url, &media))
                            .size(px(24.))
                            .object_fit(ObjectFit::Contain)
                            .into_any_element(),
                    };
                    row.h(px(40.))
                        .px(px(8.))
                        .py(px(6.))
                        .child(
                            div()
                                .size(px(28.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(preview),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(format!(":{}:", candidate.shortcode())),
                        )
                        .children(detail.map(|detail| {
                            div()
                                .flex_none()
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child(detail)
                        }))
                        .into_any_element()
                }
            }
        });
        let retry_index = completion.items.len();
        let retry = completion.retry.map(|_| {
            div()
                .id("completion-retry")
                .flex()
                .items_center()
                .px(px(8.))
                .py(px(6.))
                .rounded(px(3.))
                .cursor_pointer()
                .when(completion.selected == retry_index, |d| {
                    d.bg(c.affordance_popover_selected)
                })
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(completion) = &mut this.completion
                        && completion.selected != retry_index
                    {
                        completion.selected = retry_index;
                        cx.notify();
                    }
                }))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.accept(retry_index, window, cx)),
                )
                .child("Retry suggestions")
        });
        let status = completion.status.clone().map(|status| {
            div()
                .flex_none()
                .mx(px(8.))
                .my(px(4.))
                .text_caption()
                .text_color(c.text_subtle)
                .child(status)
        });
        let (max_width, max_height, padding) = match trigger {
            Trigger::Mention => (px(380.), px(280.), px(12.)),
            Trigger::Channel => (px(480.), px(320.), px(12.)),
            Trigger::Emoji => (px(320.), px(280.), px(6.)),
            Trigger::Plugin(_) => (px(380.), px(280.), px(12.)),
        };
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_full()
                .pb(px(4.))
                .child(
                    div()
                        .id("composer-completions")
                        .occlude()
                        .w_full()
                        .max_w(max_width)
                        .p(padding)
                        .rounded(px(16.))
                        .border_1()
                        .border_color(c.border_standard)
                        .bg(c.surface_popover)
                        .shadow(buzz_ui::tokens::shadow::sm())
                        .text_body()
                        .text_color(c.text_standard)
                        .child(
                            div()
                                .id("composer-completion-list")
                                .flex()
                                .flex_col()
                                .gap(px(if compact { 0. } else { 4. }))
                                .max_h(max_height)
                                .overflow_y_scroll()
                                .track_scroll(&self.completion_scroll)
                                .children(rows)
                                .children(retry),
                        )
                        .children(status),
                )
                .into_any_element(),
        )
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A plugin capture ends when the composer is disabled, as the
        // recorder's does (`if (disabled) cancel()`).
        if matches!(self.capture, Some(Capture::Plugin(_))) && self.disabled(cx) {
            self.capture = None;
        }
        let completions = super::composer_completions::generation();
        if completions != self.completions_generation {
            self.completions_generation = completions;
            if self
                .completion
                .as_ref()
                .is_some_and(|completion| matches!(completion.query.trigger, Trigger::Plugin(_)))
                || self.completion.is_none()
            {
                self.refresh_completion(cx);
            }
        }
        self.mount_tools(window, cx);
        let c = *cx.colors();
        self.sync_uploads(cx);
        self.markdown.follow_theme(&self.input, &c, cx);
        // The caret can move without an edit.
        let key = {
            let state = self.input.read(cx);
            (state.value().len(), state.selected_range().end)
        };
        if self.completion_key != Some(key) {
            if !self.pending.is_empty() && self.completion_key.is_some_and(|(len, _)| len == key.0)
            {
                self.leave_pending(window, cx);
            }
            if self.completion_key.is_some_and(|(len, _)| len == key.0) {
                // The caret moved: the toolbar follows it.
                self.restyle(cx);
            }
            self.refresh_completion(cx);
        }
        if self.drag_over && !cx.has_active_drag() {
            self.drag_over = false;
        }
        self.refresh_run_models(cx);
        let can_submit = self.can_submit(cx);
        let editing = self.editing.is_some();
        let has_attachments = self.has_files(cx) && !editing;
        let weak = cx.entity().downgrade();
        let token_colors = c;
        let large_emoji = {
            let community = self.community.read(cx);
            large_emoji(
                self.input.read(cx).value().as_ref(),
                &community.snapshot.emoji.entries,
            )
        };
        let chip_names = {
            let community = self.community.read(cx);
            let mut scope: Vec<String> = self
                .summary(cx)
                .map(|channel| channel.members.clone())
                .unwrap_or_default();
            scope.extend(self.recipients(cx).into_iter().map(|(key, _)| key));
            crate::ui::identity::names(
                &community.snapshot.profiles,
                &crate::ui::identity::AgentLibrary::from_hosts(cx),
                community.session.viewer(),
                Some(&community.origin),
                Some(&scope),
            )
        };
        let textarea = Textarea::new(&self.input)
            .appearance(false)
            .disabled(self.input_disabled(cx))
            // An emoji-only draft shows at the large emoji size
            // (`data-single-emoji`).
            .when(large_emoji, |t| t.text_size(px(42.)).line_height(px(48.)))
            .token(move |token: &InlineTokenContext, _, _| {
                // A recipient chip names its key in context: a namesake that
                // joins later qualifies it (`RichComposerInput`).
                let label: SharedString = match token.token().text().strip_prefix('@') {
                    Some(_) if token.token().id().len() == 64 => {
                        format!("@{}", chip_names.display(token.token().id())).into()
                    }
                    _ => token.token().label().clone(),
                };
                // A resource link reads as underlined text (`.resource`).
                let resource = token.token().id().starts_with("buzz:");
                div()
                    .flex()
                    .items_center()
                    .h(token.line_height())
                    .when(!resource, |d| d.max_w(px(256.)).px(px(3.)))
                    .rounded(px(4.))
                    .bg(if token.is_selected() {
                        token_colors.affordance_subtle_hover
                    } else if resource {
                        gpui_kit::transparent_black()
                    } else {
                        token_colors.affordance_subtle
                    })
                    .when(resource, |d| d.underline())
                    .text_color(token_colors.text_standard)
                    .overflow_hidden()
                    .child(label)
            })
            .on_paste(move |item, window, cx| {
                weak.update(cx, |this, cx| this.paste(item, window, cx))
                    .unwrap_or(false)
            });

        let shell = div()
            .id("composer")
            .key_context("Composer")
            .flex()
            .flex_col()
            .gap(px(if has_attachments { 8. } else { 16. }))
            .pt(px(if has_attachments { 6. } else { 12. }))
            .px(px(6.))
            .pb(px(6.))
            .rounded(px(COMPOSER_RADIUS))
            .border_1()
            .border_color(if self.drag_over {
                c.border_focus
            } else {
                c.border_standard
            })
            .bg(if self.drag_over {
                c.surface_inset
            } else {
                c.surface_panel
            })
            .shadow_xs()
            .when(editing, |d| d.overflow_hidden())
            .text_body()
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.completion.is_some() {
                    this.move_selection(-1, cx);
                    cx.stop_propagation();
                } else if this.editing.is_none()
                    && !this.has_files(cx)
                    && this.recipients(cx).is_empty()
                    && this.input.read(cx).value().is_empty()
                    && this.input.read(cx).focus_handle(cx).is_focused(window)
                {
                    cx.emit(ComposerEvent::EditLast);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.completion.is_some() {
                    this.move_selection(1, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if let Some(completion) = &this.completion
                    && !action.shift
                    && completion.count() > 0
                {
                    let index = completion.selected;
                    this.accept(index, window, cx);
                    cx.stop_propagation();
                } else if !action.shift && this.block_edit(blocks::enter, window, cx) {
                    // Enter in a list continues it instead of sending.
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if let Some(completion) = &this.completion
                    && completion.count() > 0
                {
                    let index = completion.selected;
                    this.accept(index, window, cx);
                    cx.stop_propagation();
                } else if this.block_edit(blocks::sink, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                if this.completion.as_ref().is_none_or(|c| c.count() == 0)
                    && this.block_edit(blocks::lift_items, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &InsertLink, window, cx| this.open_link(window, cx)))
            .capture_action(cx.listener(|this, _: &Copy, window, cx| {
                if this.copy(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Cut, window, cx| {
                if this.copy(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                if this.block_edit(blocks::backspace, window, cx) {
                    cx.stop_propagation();
                }
            }))
            // Holding Enter repeats it; a repeat never sends (`event.repeat`).
            .capture_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.is_held
                    && event.keystroke.key == "enter"
                    && !event.keystroke.modifiers.shift
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if let Some(completion) = this.completion.take() {
                    this.dismissed = Some(completion.query.start);
                    cx.stop_propagation();
                    cx.notify();
                } else if this.picker_open {
                    this.picker_open = false;
                    cx.stop_propagation();
                    cx.notify();
                } else if this.formatting {
                    this.toggle_formatting(false, true, window, cx);
                    cx.stop_propagation();
                } else if this.editing.is_some() {
                    this.cancel_edit(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.shortcut(event, window, cx)
            }))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                    // Over the composer; leaving it is the pane's to report
                    // when it is a drop zone, or the drag's end.
                    if event.bounds.contains(&event.event.position) {
                        this.set_drag_over(true, cx);
                    }
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drag_over = false;
                if this.accepts_drop() && !this.no_attachments {
                    this.add_paths(paths.paths().to_vec(), cx);
                }
                // The pane's drop zone must not attach them again.
                cx.stop_propagation();
            }))
            .when(!self.accessories.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .children(self.accessories.iter().map(|(_, _, view)| view.clone())),
                )
            })
            .when(editing, |d| d.child(self.edit_header(cx)))
            .children(self.edit_retry(cx))
            .children(self.background_status(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .min_w_0()
                    // The tiles align with the toolbar, not the text.
                    .when(!has_attachments, |d| d.pl(px(4.)).pt(px(4.)))
                    .children(self.attachment_row(window, cx))
                    .child(
                        div()
                            .min_w_0()
                            .when(has_attachments, |d| d.pl(px(4.)))
                            .child(textarea),
                    ),
            )
            .children(self.notice.clone().map(|notice| {
                div()
                    .px(px(8.))
                    .text_caption()
                    .text_color(c.text_warning)
                    .child(notice)
            }))
            .children(self.context_box(cx))
            .children(self.capture_row(window, cx))
            .when(self.capture.is_none(), |d| {
                // Me keeps the tools and Send on one row, with its
                // controls on the row below.
                let single = self.chat || self.trailing.is_some();
                let trailing = self.trailing.clone().filter(|_| !editing);
                d.child(
                    div()
                        .flex()
                        .when(!single, |d| d.flex_wrap())
                        .items_center()
                        .gap(px(8.))
                        .child(self.tools(cx))
                        .children(self.run_models_button(cx))
                        .child(
                            IconButton::new("send", IconName::ArrowUp)
                                .size(IconButtonSize::Sm)
                                .when(can_submit, |b| b.variant(ButtonVariant::Prominent))
                                .disabled(!can_submit)
                                .tooltip(if editing {
                                    "Save changes"
                                } else {
                                    "Send message"
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                        ),
                )
                .when_some(trailing, |d, trailing| {
                    d.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                            .w_full()
                            .child(trailing),
                    )
                })
            })
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(8.))
                    .text_body()
                    .text_color(c.text_standard)
                    .child(error)
            }))
            .children(self.draft_status(cx))
            .children(self.preparation_retry(cx));

        let picker = self.picker_open.then(|| {
            let dismiss = cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.picker_open = false;
                this.picker_closed = Some(std::time::Instant::now());
                cx.notify();
            });
            div().absolute().left_0().top_0().child(deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(0.), px(-4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(emoji_picker::emoji_picker(&self.picker, dismiss)),
            ))
        });

        // The picker fits the composer's width (`EmojiPicker` layout).
        let fit = self.picker.clone();
        let chat = self.chat;
        let composer = div()
            .relative()
            // The attachment row scrolls sideways, which lets a crowded
            // pane squeeze it to nothing; the composer keeps its height.
            .flex_none()
            .mx(px(16.))
            .mb(if chat { px(12.) } else { px(16.) })
            // Me's composer sits at the transcript's measure.
            .when(chat, |d| d.w_full().max_w(rems(52.)))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        if fit.update(cx, |picker, _| picker.fit(width)) {
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(self.completion_popup(cx))
            .children(self.mention_picker_popup(cx))
            .children(self.run_models_popup(cx))
            .children(picker)
            .child(shell)
            .when(!self.zoned, |d| d.children(drop_overlay(self, cx)))
            .children(self.toasts(window, cx))
            .children(self.outsiders_dialog(window, cx))
            .children(self.link.as_ref().map(|(dialog, _)| {
                overlay::modal(window, dialog.clone(), None).into_any_element()
            }));
        if chat {
            div()
                .flex()
                .justify_center()
                .w_full()
                .child(composer)
                .into_any_element()
        } else {
            composer.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn query(text: &str) -> Option<(Trigger, String)> {
        find_query(text, text.len(), &[('/', "x/commands")]).map(|q| (q.trigger, q.text))
    }

    fn channel(channel_type: ChannelType, members: &[&str]) -> ChannelSummary {
        ChannelSummary {
            id: "c".into(),
            channel_type: Some(channel_type),
            members: members.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    fn person(key: &str, member: bool) -> Suggestion {
        Suggestion::Person {
            pubkey: key.into(),
            name: key.to_uppercase().into(),
            label: key.to_uppercase().into(),
            picture: None,
            agent: false,
            member,
            dm: false,
            disabled: None,
        }
    }

    #[test]
    fn shown_people_stay_and_say_why_they_cannot_be_picked() {
        let old = vec![person("a", true), person("b", false), person("c", true)];
        let new = vec![person("d", true), person("c", false), person("a", true)];
        let kept = keep_shown(
            &old,
            new,
            |key| if key == "b" { "Archived" } else { "gone" },
        );
        let rows: Vec<(&str, Option<&str>)> = kept
            .iter()
            .map(|item| match item {
                Suggestion::Person {
                    pubkey, disabled, ..
                } => (pubkey.as_str(), disabled.as_ref().map(|d| d.as_ref())),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("a", None),
                ("b", Some("Archived")),
                (
                    "c",
                    Some("Channel membership changed. Reopen to review adding this recipient.")
                ),
                ("d", None),
            ]
        );
    }

    #[test]
    fn asks_before_mentioning_outsiders_in_streams_and_forums() {
        let stream = channel(ChannelType::Stream, &["me", "bob"]);
        assert_eq!(
            admission(Some(&stream), "me", keys(&["bob"])),
            Admission::Send(Mentions {
                recipients: keys(&["bob"]),
                references: Vec::new(),
                ..Default::default()
            })
        );
        assert_eq!(
            admission(Some(&stream), "me", keys(&["bob", "carol"])),
            Admission::Ask {
                missing: keys(&["carol"]),
                can_add: true,
            }
        );
        // Not a member: only "Send anyway" is offered.
        let forum = channel(ChannelType::Forum, &["bob"]);
        assert_eq!(
            admission(Some(&forum), "me", keys(&["carol"])),
            Admission::Ask {
                missing: keys(&["carol"]),
                can_add: false,
            }
        );
        let archived = ChannelSummary {
            archived: true,
            ..channel(ChannelType::Stream, &["me"])
        };
        assert_eq!(
            admission(Some(&archived), "me", keys(&["carol"])),
            Admission::Ask {
                missing: keys(&["carol"]),
                can_add: false,
            }
        );
    }

    #[test]
    fn references_outsiders_in_dms_without_asking() {
        let dm = channel(ChannelType::Dm, &["me", "bob"]);
        assert_eq!(
            admission(Some(&dm), "me", keys(&["carol", "bob"])),
            Admission::Send(Mentions {
                recipients: keys(&["bob"]),
                references: keys(&["carol"]),
                ..Default::default()
            })
        );
        // An unknown roster leaves the decision to the writer's check.
        assert_eq!(
            admission(None, "me", keys(&["carol"])),
            Admission::Send(Mentions {
                recipients: keys(&["carol"]),
                references: Vec::new(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn finds_completion_queries_like_the_reference() {
        assert_eq!(query("hi @al"), Some((Trigger::Mention, "al".into())));
        assert_eq!(query("@"), Some((Trigger::Mention, String::new())));
        assert_eq!(
            query("(@Alice Sm"),
            Some((Trigger::Mention, "Alice Sm".into()))
        );
        assert_eq!(query("mail@host"), None);
        assert_eq!(query("see #gen"), Some((Trigger::Channel, "gen".into())));
        assert_eq!(query("a#b"), None);
        assert_eq!(query(":smi"), Some((Trigger::Emoji, "smi".into())));
        assert_eq!(query("ok :s"), None, "two characters at least");
        assert_eq!(query("at 12:30"), None);
        assert_eq!(query("@al\nnext"), None);
        let text = "x @bo";
        assert_eq!(find_query(text, text.len(), &[]).unwrap().start, 2);
        // A plugin's trigger, up to the first space.
        assert_eq!(
            query("/dep"),
            Some((Trigger::Plugin("x/commands"), "dep".into()))
        );
        assert_eq!(
            query("run /"),
            Some((Trigger::Plugin("x/commands"), String::new()))
        );
        assert_eq!(query("/dep now"), None);
        assert_eq!(query("a/b"), None);
    }

    #[test]
    fn edits_channel_references_as_chips() {
        let link = channel_reference("b8fb-1", "random");
        let text = format!("about {link} now");
        let content = edit_content(&text);
        assert_eq!(content.tokens().len(), 1);
        let span = &content.tokens()[0];
        assert_eq!(span.token().id().as_ref(), "b8fb-1");
        assert_eq!(span.token().label().as_ref(), "#random");
        assert_eq!(&text[span.range()], link);
        assert!(edit_content("[\\#x](https://x)").tokens().is_empty());
    }

    #[test]
    fn ranks_names() {
        assert_eq!(name_match("Alice", ""), Some(0));
        assert_eq!(name_match("Alice", "alice"), Some(0));
        assert_eq!(name_match("Alice", "al"), Some(1));
        assert_eq!(name_match("Mary Alice", "ali"), Some(2));
        assert_eq!(name_match("Malice", "lic"), Some(3));
        assert_eq!(name_match("Bob", "x"), None);
    }

    #[test]
    fn references_channels_as_markdown_links() {
        assert_eq!(
            channel_reference("0e9d-1", "general_chat"),
            "[\\#general\\_chat](buzz://channel/0e9d-1)"
        );
    }

    #[test]
    fn replays_a_missed_recovery_only_onto_its_own_draft() {
        let sent = r#"{"text":"think hard@Honey ","tokens":[]}"#;
        assert!(recovery_owns(Some(sent), sent));
        // A replacement written while the composer was away keeps neither
        // the old error nor the old picks.
        assert!(!recovery_owns(
            Some(r#"{"text":"later","tokens":[]}"#),
            sent
        ));
        assert!(!recovery_owns(None, sent));
    }
}
