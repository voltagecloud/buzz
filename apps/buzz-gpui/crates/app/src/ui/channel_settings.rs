//! "Channel settings" (`ChannelSettingsPanel.tsx`): the channel's identity,
//! its details (Description, Visibility, Members, Channel ID), the details
//! editor (`ChannelDetailsEditor.tsx`) and the lifecycle buttons
//! (`ChannelLifecycleActions.tsx`). The lifecycle confirmation
//! (`ChannelLifecycleDialog.tsx`) is shared with the header menu.
//!
//! The panel renders only its body. Its host draws the "Channel settings"
//! tab (gear icon, close button) around it and passes `on_close`, which
//! Escape inside the panel also calls.

use super::channel_diagnostics::ChannelDiagnostics;
use super::form_fields::{self, Bounded};
use super::outbox_status::OutboxStatus;
use super::overlay::{self, Dismiss};
use super::sidebar::channel_label;
use crate::community::Community;
use buzz_relay_client::lifecycle::{
    Action, ActionError, AttemptStatus, ChannelDetails, DEFAULT_TEMPORARY_TTL, DetailsAttempt,
    DetailsDraft, DraftErrors, LifecyclePermissions, canonical_name, draft_errors,
};
use buzz_relay_client::{ChannelSummary, ChannelType, Visibility};
use buzz_ui::{
    ActiveTheme, AlertDialog, Button, Dialog, Icon, IconName, Spinner, Surface, Switch, Tooltip,
    TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub type Callback = Rc<dyn Fn(&mut Window, &mut App)>;

/// The viewer left or deleted the channel from the panel; its page
/// should close.
pub struct ChannelRemoved;

// ---------------------------------------------------------------------------
// Lifecycle confirmation

/// How a [`LifecycleDialog`] ended.
pub enum LifecycleOutcome {
    Cancelled,
    Completed(Action),
}

/// `ChannelLifecycleDialog.tsx`: "Archive channel: name" with its detail,
/// Cancel and the action. Busy blocks closing; after an unconfirmed result
/// the action stays disabled so the command is never resent from here.
pub struct LifecycleDialog {
    community: Entity<Community>,
    channel: String,
    name: SharedString,
    action: Action,
    busy: bool,
    error: Option<SharedString>,
    refresh_required: bool,
    focus: FocusHandle,
}

impl EventEmitter<LifecycleOutcome> for LifecycleDialog {}

/// The title and detail of each confirmation.
pub fn lifecycle_copy(action: Action) -> (&'static str, &'static str) {
    match action {
        Action::Archive => (
            "Archive channel",
            "Archive this channel for everyone and remove it from the sidebar. Messages are \
             kept. Find it in search and open Settings to unarchive it later.",
        ),
        Action::Unarchive => (
            "Unarchive channel",
            "Unarchive this channel for everyone and return it to the sidebar. Members can \
             send messages again.",
        ),
        Action::Leave => (
            "Leave channel",
            "Leave this channel and remove it from your sidebar. You may need an invitation \
             to rejoin a private channel.",
        ),
        Action::Hide => (
            "Hide conversation",
            "Hide this conversation from your sidebar only. Messages and membership are \
             kept; other participants are not removed.",
        ),
        Action::Delete => (
            "Delete channel",
            "Delete this channel for everyone. You cannot undo this action from Buzz.",
        ),
        Action::Join => ("Join channel", ""),
    }
}

impl LifecycleDialog {
    pub fn new(
        community: Entity<Community>,
        channel: &ChannelSummary,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = channel_label(channel, &community.read(cx).snapshot.profiles);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            community,
            channel: channel.id.clone(),
            name,
            action,
            busy: false,
            error: None,
            refresh_required: false,
            focus,
        }
    }

    /// Cancel, the scrim and Escape; ignored while the command runs.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(LifecycleOutcome::Cancelled);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.refresh_required {
            return;
        }
        self.busy = true;
        self.error = None;
        let session = self.community.read(cx).session.clone();
        let (action, channel) = (self.action, self.channel.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = session.channel_action(action, &channel).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => cx.emit(LifecycleOutcome::Completed(action)),
                    Err(failure) => {
                        this.refresh_required = matches!(failure, ActionError::Unconfirmed(_));
                        this.error = Some(failure.to_string().into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for LifecycleDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, detail) = lifecycle_copy(self.action);
        // Only Delete is destructive.
        let destructive = self.action == Action::Delete;
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.cancel(cx);
                }
            }))
            .child(
                AlertDialog::new(format!("{title}: {}", self.name))
                    .description(detail)
                    .children(
                        self.error
                            .clone()
                            .map(|message| div().text_body_sm().child(message)),
                    )
                    .action(
                        Button::new("lifecycle-cancel", "Cancel")
                            .surface(Surface::Floating)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .action(
                        Button::new("lifecycle-confirm", title)
                            .map(|b| {
                                if destructive {
                                    b.destructive()
                                } else {
                                    b.prominent()
                                }
                            })
                            .loading(self.busy)
                            .disabled(self.refresh_required)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Shows a lifecycle dialog over the window; the scrim cancels it.
pub fn lifecycle_modal(dialog: &Entity<LifecycleDialog>, window: &Window) -> AnyElement {
    let weak = dialog.downgrade();
    let dismiss: Dismiss = Rc::new(move |_, cx| {
        let _ = weak.update(cx, |dialog, cx| dialog.cancel(cx));
    });
    overlay::modal(window, dialog.clone(), Some(dismiss)).into_any_element()
}

// ---------------------------------------------------------------------------
// Details editor state

/// Which field the editor focuses when it opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditField {
    #[default]
    Name,
    Description,
}

/// `ChannelDetailsEditor`'s view state, apart from the text inputs. The
/// session owns any save in flight ([`DetailsAttempt`]); every method takes
/// it so a reopened panel shows that save.
#[derive(Clone, Debug, Default)]
struct Details {
    loading: bool,
    base: Option<ChannelDetails>,
    /// For comparison only: kept through failed reads, never authority.
    baseline_ttl: Option<u64>,
    draft: Option<DetailsDraft>,
    editing: bool,
    focus: EditField,
    confirm_discard: bool,
    /// The visibility awaiting "Make channel private/public?", `true` for
    /// private.
    privacy_choice: Option<bool>,
    skip_warning: bool,
    pending: bool,
    /// The temporary duration kept while Ongoing is chosen.
    temporary_ttl: Option<u64>,
    error: Option<String>,
}

impl Details {
    fn current<'a>(&'a self, attempt: Option<&'a DetailsAttempt>) -> Option<&'a DetailsDraft> {
        attempt
            .map(|attempt| &attempt.draft)
            .or(self.draft.as_ref())
    }

    fn normalized(&self, attempt: Option<&DetailsAttempt>) -> Option<DetailsDraft> {
        self.current(attempt).map(|draft| DetailsDraft {
            name: canonical_name(&draft.name),
            ..draft.clone()
        })
    }

    fn errors(&self, attempt: Option<&DetailsAttempt>) -> DraftErrors {
        self.normalized(attempt)
            .map(|draft| draft_errors(&draft.name, &draft.description, draft.ttl))
            .unwrap_or_default()
    }

    fn pending(&self, attempt: Option<&DetailsAttempt>) -> bool {
        self.pending || attempt.is_some_and(|attempt| attempt.status == AttemptStatus::Saving)
    }

    fn locked(&self, attempt: Option<&DetailsAttempt>) -> bool {
        self.loading || self.pending(attempt) || attempt.is_some()
    }

    fn can_edit(&self) -> bool {
        self.base.as_ref().is_some_and(|base| base.can_edit)
    }

    fn confirming_discard(&self, attempt: Option<&DetailsAttempt>) -> bool {
        self.confirm_discard && attempt.is_none()
    }

    fn dirty(&self, attempt: Option<&DetailsAttempt>) -> bool {
        match (self.normalized(attempt), &self.base) {
            (Some(draft), Some(base)) => draft != base.draft,
            _ => false,
        }
    }

    fn can_save(&self, attempt: Option<&DetailsAttempt>) -> bool {
        self.editing
            && self.privacy_choice.is_none()
            && !self.confirming_discard(attempt)
            && !self.locked(attempt)
            && self.can_edit()
            && self.dirty(attempt)
            && !self.errors(attempt).any()
    }

    /// The text as typed differs from the details, even when normalization
    /// would make Save a no-op. A failed reload keeps the draft protected.
    fn unsaved(&self, attempt: Option<&DetailsAttempt>) -> bool {
        attempt.is_none()
            && self
                .draft
                .as_ref()
                .is_some_and(|draft| self.base.as_ref().is_none_or(|base| *draft != base.draft))
    }

    fn temporary_ttl(&self, attempt: Option<&DetailsAttempt>) -> u64 {
        self.current(attempt)
            .and_then(|draft| draft.ttl)
            .or(self.temporary_ttl)
            .or(self.base.as_ref().and_then(|base| base.draft.ttl))
            .unwrap_or(DEFAULT_TEMPORARY_TTL)
    }

    /// Whether the edit dialog may open, from the panel's actions.
    fn editable(&self, attempt: Option<&DetailsAttempt>) -> bool {
        !self.pending(attempt) && (attempt.is_some() || (!self.loading && self.can_edit()))
    }

    /// Opens the dialog on the save in flight, else the current details.
    fn edit(&mut self, field: EditField, attempt: Option<&DetailsAttempt>) -> bool {
        if !self.editable(attempt) {
            return false;
        }
        self.editing = true;
        self.focus = field;
        self.confirm_discard = false;
        self.draft = attempt
            .map(|attempt| attempt.draft.clone())
            .or_else(|| self.base.as_ref().map(|base| base.draft.clone()));
        self.temporary_ttl = None;
        self.error = None;
        true
    }

    fn close(&mut self, attempt: Option<&DetailsAttempt>) {
        if !self.pending(attempt) {
            self.editing = false;
            self.error = None;
            self.privacy_choice = None;
            self.confirm_discard = false;
        }
    }

    /// Close, Escape and the scrim step back: privacy, then discard, then
    /// the dialog.
    fn request_close(&mut self, attempt: Option<&DetailsAttempt>) {
        if self.pending(attempt) {
            return;
        }
        if self.privacy_choice.is_some() {
            self.privacy_choice = None;
        } else if self.confirming_discard(attempt) {
            self.confirm_discard = false;
        } else if self.unsaved(attempt) {
            self.confirm_discard = true;
        } else {
            self.close(attempt);
        }
    }

    fn loading(&mut self) {
        self.loading = true;
        self.error = None;
        self.privacy_choice = None;
    }

    /// A fresh read keeps explicit choices but adopts the relay's
    /// visibility and lifetime for fields left alone.
    fn loaded(&mut self, base: ChannelDetails) {
        if let Some(draft) = &mut self.draft {
            let chose_privacy = self
                .base
                .as_ref()
                .is_some_and(|old| draft.private != old.draft.private);
            if !chose_privacy {
                draft.private = base.draft.private;
            }
            if draft.ttl == self.baseline_ttl {
                draft.ttl = base.draft.ttl;
            }
        }
        self.baseline_ttl = base.draft.ttl;
        self.base = Some(base);
        self.loading = false;
    }

    fn failed(&mut self, message: String) {
        self.base = None;
        self.error = Some(message);
        self.loading = false;
    }
}

// ---------------------------------------------------------------------------
// Lifecycle buttons state

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retry {
    Delete,
    All,
}

/// `ChannelLifecycleActions`: permissions read while the panel is open.
#[derive(Default)]
struct Lifecycle {
    /// `None` while the first read runs.
    permissions: Option<Result<LifecyclePermissions, String>>,
    /// A retry in flight keeps its button.
    retrying: Option<Retry>,
    retries: usize,
    generation: u64,
    /// The archive state the permissions were read for.
    archived: Option<bool>,
}

// ---------------------------------------------------------------------------
// Panel

pub struct ChannelSettingsPanel {
    community: Entity<Community>,
    channel_id: String,
    /// Used when the channel is not in the list, e.g. a read-only preview.
    fallback: Option<ChannelSummary>,
    on_close: Callback,
    open_members: Callback,
    focus: FocusHandle,
    dialog_focus: FocusHandle,
    details: Details,
    details_started: bool,
    details_generation: u64,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    name_bound: Bounded,
    description_bound: Bounded,
    lifecycle: Lifecycle,
    confirm: Option<(Entity<LifecycleDialog>, Subscription)>,
    copied: bool,
    /// The Diagnostics disclosure, its unread, live and refresh tools,
    /// and the session outbox inside it.
    diagnostics: bool,
    tools: Entity<ChannelDiagnostics>,
    outbox: Entity<OutboxStatus>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ChannelRemoved> for ChannelSettingsPanel {}

impl Focusable for ChannelSettingsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Streams and forums the viewer has joined and may edit from here.
fn editable(channel: &ChannelSummary) -> bool {
    matches!(
        channel.channel_type,
        Some(ChannelType::Stream | ChannelType::Forum)
    ) && !channel.read_only
        && !channel.cached
        && !channel.archived
}

/// Leave, Archive and Delete belong to joined non-DM channels.
fn shows_lifecycle(channel: &ChannelSummary) -> bool {
    !channel.read_only && channel.channel_type != Some(ChannelType::Dm)
}

const UNCONFIRMED_NOTE: &str = "The change may have been saved. Check its status before trying \
                                again. Checking never resends it. Closing this dialog does not \
                                undo the change.";

impl ChannelSettingsPanel {
    pub fn new(
        community: Entity<Community>,
        channel_id: String,
        on_close: Callback,
        open_members: Callback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("release-notes"));
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("What this channel is for")
                .auto_grow(3, 8)
        });
        let name_bound = Bounded::new(&name, form_fields::TextKind::Name.limit(), window, cx);
        let description_bound = Bounded::new(
            &description,
            form_fields::TextKind::Description.limit(),
            window,
            cx,
        );
        let subscriptions = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, state, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        let value = state.read(cx).value().to_string();
                        if let Some(draft) = &mut this.details.draft {
                            draft.name = value;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.save(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe(&description, |this, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = state.read(cx).value().to_string();
                    if let Some(draft) = &mut this.details.draft {
                        draft.description = value;
                    }
                    cx.notify();
                }
            }),
            cx.observe_in(&community, window, |this, _, window, cx| {
                this.sync(window, cx)
            }),
        ];
        let session = community.read(cx).session.clone();
        let outbox = cx.new(|cx| OutboxStatus::new(session, cx));
        let tools = cx.new(|cx| ChannelDiagnostics::new(community.clone(), channel_id.clone(), cx));
        let mut panel = Self {
            community,
            channel_id,
            fallback: None,
            on_close,
            open_members,
            focus: cx.focus_handle(),
            dialog_focus: cx.focus_handle(),
            details: Details::default(),
            details_started: false,
            details_generation: 0,
            name,
            description,
            name_bound,
            description_bound,
            lifecycle: Lifecycle::default(),
            confirm: None,
            copied: false,
            diagnostics: false,
            tools,
            outbox,
            _subscriptions: subscriptions,
        };
        panel.sync(window, cx);
        panel
    }

    /// The summary to show when the channel list lacks it (a preview).
    pub fn with_channel(
        mut self,
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        self.fallback = Some(channel);
        self.sync(window, cx);
        self
    }

    fn channel(&self, cx: &App) -> Option<ChannelSummary> {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel_id)
            .cloned()
            .or_else(|| self.fallback.clone())
    }

    fn attempt(&self, cx: &App) -> Option<DetailsAttempt> {
        self.community
            .read(cx)
            .session
            .details_attempt(&self.channel_id)
    }

    /// Starts the reads the channel's current state calls for.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel(cx) else {
            return;
        };
        if editable(&channel) && !self.details_started {
            self.details_started = true;
            self.load_details(window, cx);
        }
        // `key={id:archived}`: archiving reads the permissions afresh.
        if shows_lifecycle(&channel) && self.lifecycle.archived != Some(channel.archived) {
            self.lifecycle = Lifecycle {
                archived: Some(channel.archived),
                ..Lifecycle::default()
            };
            self.load_permissions(window, cx);
        }
    }

    // ----- details -----

    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.details.loading();
        self.details_generation += 1;
        let generation = self.details_generation;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.channel_details(&channel).await;
            let _ = this.update(cx, |this, cx| {
                if this.details_generation != generation {
                    return;
                }
                match result {
                    Ok(base) => this.details.loaded(base),
                    Err(message) => this.details.failed(message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_editor(&mut self, field: EditField, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if !self.details.edit(field, attempt.as_ref()) {
            return;
        }
        if let Some(draft) = self.details.draft.clone() {
            self.name_bound.set(&self.name, &draft.name, window, cx);
            self.description_bound
                .set(&self.description, &draft.description, window, cx);
        }
        self.focus_field(window, cx);
        cx.notify();
    }

    fn focus_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.details.focus {
            EditField::Name => self.name.update(cx, |s, cx| s.focus(window, cx)),
            EditField::Description => self.description.update(cx, |s, cx| s.focus(window, cx)),
        }
    }

    /// Moves focus to match the dialog's step: the form's field, or the
    /// step itself so Escape steps back.
    fn after_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if !self.details.editing {
            self.focus.focus(window, cx);
        } else if self.details.privacy_choice.is_some()
            || self.details.confirming_discard(attempt.as_ref())
        {
            self.dialog_focus.focus(window, cx);
        } else {
            self.focus_field(window, cx);
        }
        cx.notify();
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        self.details.request_close(attempt.as_ref());
        self.after_step(window, cx);
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        self.details.close(attempt.as_ref());
        self.after_step(window, cx);
    }

    fn set_private(&mut self, private: bool, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if self.details.locked(attempt.as_ref()) || !self.details.can_edit() {
            return;
        }
        let skip = form_fields::skip_privacy_confirmation(self.community.read(cx), cx);
        if skip {
            if let Some(draft) = &mut self.details.draft {
                draft.private = private;
            }
        } else {
            self.details.privacy_choice = Some(private);
            self.details.skip_warning = false;
        }
        self.after_step(window, cx);
    }

    fn confirm_privacy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        let Some(private) = self.details.privacy_choice else {
            return;
        };
        if self.details.locked(attempt.as_ref()) || !self.details.can_edit() {
            return;
        }
        if self.details.skip_warning {
            form_fields::remember_privacy_confirmation(self.community.read(cx), cx);
        }
        if let Some(draft) = &mut self.details.draft {
            draft.private = private;
        }
        self.details.privacy_choice = None;
        self.after_step(window, cx);
    }

    fn set_duration(&mut self, ttl: Option<u64>, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if self.details.locked(attempt.as_ref()) || !self.details.can_edit() {
            return;
        }
        let kept = self.details.temporary_ttl(attempt.as_ref());
        if let Some(draft) = &mut self.details.draft {
            draft.ttl = ttl;
        }
        self.details.temporary_ttl = Some(ttl.unwrap_or(kept));
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if !self.details.can_save(attempt.as_ref()) {
            return;
        }
        let (Some(base), Some(draft)) = (
            self.details.base.clone(),
            self.details.normalized(attempt.as_ref()),
        ) else {
            return;
        };
        self.details.pending = true;
        self.details.error = None;
        let session = self.community.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session
                .save_channel_details(&base.channel, base.version, draft)
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.details.pending = false;
                this.finish(result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// "Check save status": re-reads, never resends.
    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.attempt(cx);
        if self.details.pending || attempt.map(|a| a.status) != Some(AttemptStatus::Unconfirmed) {
            return;
        }
        self.details.pending = true;
        self.details.error = None;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.check_channel_details(&channel).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.details.pending = false;
                this.finish(result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// After a save or check: reload and close, or show why not.
    fn finish(&mut self, result: Result<(), String>, window: &mut Window, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.details.editing = false;
                self.load_details(window, cx);
                self.after_step(window, cx);
            }
            Err(message) => self.details.error = Some(message),
        }
        cx.notify();
    }

    // ----- lifecycle -----

    fn load_permissions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lifecycle.generation += 1;
        let generation = self.lifecycle.generation;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let permissions = session.channel_permissions(&channel).await;
            let _ = this.update(cx, |this, cx| {
                if this.lifecycle.generation == generation {
                    this.lifecycle.permissions = Some(permissions);
                    this.lifecycle.retrying = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn retry_permissions(&mut self, kind: Retry, window: &mut Window, cx: &mut Context<Self>) {
        // Keep the retry control, but drop every stale permission.
        self.lifecycle.permissions = None;
        self.lifecycle.retrying = Some(kind);
        self.lifecycle.retries += 1;
        self.load_permissions(window, cx);
        cx.notify();
    }

    fn choose(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel(cx) else {
            return;
        };
        let dialog =
            cx.new(|cx| LifecycleDialog::new(self.community.clone(), &channel, action, window, cx));
        let subscription = cx.subscribe_in(
            &dialog,
            window,
            |this, _, outcome: &LifecycleOutcome, window, cx| {
                this.confirm = None;
                match outcome {
                    LifecycleOutcome::Completed(Action::Leave | Action::Delete | Action::Hide) => {
                        cx.emit(ChannelRemoved);
                    }
                    _ => this.focus.focus(window, cx),
                }
                cx.notify();
            },
        );
        self.confirm = Some((dialog, subscription));
        cx.notify();
    }

    fn copy_id(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.channel_id.clone()));
        self.copied = true;
        cx.notify();
    }
}

// ----- rendering -----

/// What a details row does when pressed.
type RowClick = Box<dyn Fn(&mut Window, &mut App)>;

/// A row of the details list. With an action the whole row is a button.
struct Row {
    id: &'static str,
    label: &'static str,
    value: SharedString,
    /// The glyph after the label, shown on hover (pencil, copy).
    icon: Option<IconName>,
    /// A trailing glyph instead (the Members caret).
    trailing: Option<IconName>,
    loading: bool,
    on_click: Option<RowClick>,
    feedback: Option<AnyElement>,
}

impl Row {
    fn new(id: &'static str, label: &'static str, value: impl Into<SharedString>) -> Self {
        Self {
            id,
            label,
            value: value.into(),
            icon: None,
            trailing: None,
            loading: false,
            on_click: None,
            feedback: None,
        }
    }

    fn render(self, first: bool, cx: &App) -> Stateful<Div> {
        let c = cx.colors();
        let group = SharedString::from(format!("settings-row-{}", self.id));
        let interactive = self.on_click.is_some() && !self.loading;
        let hover_icon = |icon: AnyElement| {
            div()
                .flex()
                .text_color(c.text_subtle)
                .when(!self.loading, |this| {
                    this.invisible().group_hover(group.clone(), |s| s.visible())
                })
                .child(icon)
        };
        let label = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(space::S3)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .child(self.label)
                    .when(self.trailing.is_none(), |this| {
                        this.children(match (self.loading, self.icon) {
                            (true, _) => {
                                Some(hover_icon(Spinner::new().size(14.).into_any_element()))
                            }
                            (false, Some(icon)) => {
                                Some(hover_icon(Icon::new(icon).size(14.).into_any_element()))
                            }
                            (false, None) => None,
                        })
                    }),
            )
            .children(
                self.trailing
                    .map(|icon| Icon::new(icon).size(16.).color(c.text_standard)),
            )
            .children(self.feedback);
        div()
            .id(self.id)
            .group(group.clone())
            .relative()
            .p(space::S4)
            .when(!first, |this| {
                this.border_t_1().border_color(c.border_standard)
            })
            .when(interactive, |this| {
                let on_click = self.on_click.expect("interactive rows have an action");
                this.cursor_pointer()
                    .hover(|s| s.bg(c.affordance_panel_hover))
                    .active(|s| s.bg(c.affordance_subtle_pressed))
                    .on_click(move |_, window, cx| on_click(window, cx))
            })
            .child(label)
            .child(
                div()
                    .mt(space::S1)
                    .text_color(c.text_subtle)
                    .child(self.value),
            )
    }
}

impl ChannelSettingsPanel {
    fn identity(
        &self,
        channel: &ChannelSummary,
        name: SharedString,
        edit: Option<bool>,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = *cx.colors();
        let icon = if channel.private {
            IconName::Lock
        } else {
            IconName::Hash
        };
        let title: AnyElement = match edit {
            // `Some(pending)`: the name opens the editor.
            Some(pending) => div()
                .id("settings-title")
                .group("settings-title")
                .flex()
                .items_center()
                .gap(space::S2)
                .px(rems(1.5))
                .when(!pending, |this| {
                    this.cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_editor(EditField::Name, window, cx)
                        }))
                })
                .child(name)
                .child(
                    div()
                        .flex()
                        .when(!pending, |this| {
                            this.invisible()
                                .group_hover("settings-title", |s| s.visible())
                        })
                        .child(if pending {
                            Spinner::new().size(16.).into_any_element()
                        } else {
                            Icon::new(IconName::PencilSimple)
                                .size(16.)
                                .into_any_element()
                        }),
                )
                .into_any_element(),
            None => div().child(name).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S4)
            .py(space::S5)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(rems(4.5))
                    .rounded(radius::PILL)
                    .bg(c.affordance_prominent)
                    .child(Icon::new(icon).size(32.).color(c.text_inverse)),
            )
            .child(
                div()
                    .max_w_full()
                    .text_heading()
                    .text_color(c.text_standard)
                    .text_center()
                    .child(title),
            )
    }

    fn status(&self, attempt: Option<&DetailsAttempt>, cx: &mut Context<Self>) -> Option<Div> {
        let unconfirmed = attempt.is_some_and(|a| a.status == AttemptStatus::Unconfirmed);
        if !unconfirmed && self.details.error.is_none() {
            return None;
        }
        let pending = self.details.pending(attempt);
        Some(
            div()
                .flex()
                .flex_col()
                .gap(space::S4)
                .text_body_sm()
                .when(unconfirmed, |this| this.child(UNCONFIRMED_NOTE))
                .children(self.details.error.clone())
                .when(attempt.is_none() && self.details.error.is_some(), |this| {
                    this.child(
                        div().child(
                            Button::new("reload-details", "Reload details")
                                .disabled(self.details.loading || pending)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.load_details(window, cx)
                                    }),
                                ),
                        ),
                    )
                }),
        )
    }

    /// "Diagnostics", closed by default, holding the session outbox.
    fn diagnostics(&self, cx: &mut Context<Self>) -> Div {
        let c = cx.colors();
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .pt(space::S6)
            .child(
                div()
                    .id("settings-diagnostics")
                    .flex()
                    .items_center()
                    .gap(space::S1)
                    .cursor_pointer()
                    .text_color(c.text_subtle)
                    .child(Icon::new(if self.diagnostics {
                        IconName::CaretDown
                    } else {
                        IconName::CaretRight
                    }))
                    .child("Diagnostics")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.diagnostics = !this.diagnostics;
                        cx.notify();
                    })),
            )
            .when(self.diagnostics, |this| {
                this.child(self.tools.clone()).child(self.outbox.clone())
            })
    }

    fn lifecycle_actions(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.lifecycle;
        let permissions = match &state.permissions {
            Some(Ok(permissions)) => Some(permissions.clone()),
            _ => None,
        };
        let failed = matches!(state.permissions, Some(Err(_)));
        let column = div().flex().flex_col().gap(space::S3);
        if state.permissions.is_none() && state.retrying.is_none() {
            return column;
        }
        let choose = |action: Action| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.choose(action, window, cx)
            })
        };
        let p = permissions.clone().unwrap_or_default();
        let recovery = state.retrying.is_some() || failed || p.delete_unavailable;
        let retry_label = if failed || state.retrying == Some(Retry::All) {
            "Retry channel permissions"
        } else {
            "Retry Delete check"
        };
        let none = state.retries > 0
            && permissions.is_some()
            && !p.can_leave
            && !p.can_archive
            && !p.can_unarchive
            && !p.can_delete
            && !p.delete_unavailable;
        let c = cx.colors();
        column
            .when(p.can_leave, |this| {
                this.child(
                    Button::new("settings-leave", "Leave channel")
                        .icon(IconName::SignOut)
                        .full_width()
                        .on_click(choose(Action::Leave)),
                )
            })
            .when(p.can_archive || p.can_unarchive, |this| {
                let (label, action) = if p.can_unarchive {
                    ("Unarchive channel", Action::Unarchive)
                } else {
                    ("Archive channel", Action::Archive)
                };
                this.child(
                    Button::new("settings-archive", label)
                        .icon(IconName::Archive)
                        .full_width()
                        .on_click(choose(action)),
                )
            })
            .when(recovery, |this| {
                let kind = if failed { Retry::All } else { Retry::Delete };
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(space::S2)
                        // While retrying the status is for screen readers only.
                        .when(state.retrying.is_none(), |this| {
                            this.child(div().text_body_sm().text_color(c.text_standard).child(
                                if failed {
                                    "Channel actions unavailable"
                                } else {
                                    "Delete check unavailable"
                                },
                            ))
                        })
                        .child(
                            Button::new("settings-retry", retry_label)
                                .loading(state.retrying.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.retry_permissions(kind, window, cx)
                                })),
                        ),
                )
            })
            .when(p.can_delete, |this| {
                this.child(
                    Button::new("settings-delete", "Delete channel")
                        .icon(IconName::Trash)
                        .destructive()
                        .full_width()
                        .on_click(choose(Action::Delete)),
                )
            })
            .when(none, |this| {
                this.child(div().text_body_sm().child("No channel actions available."))
            })
    }

    fn editor_dialog(
        &self,
        attempt: Option<&DetailsAttempt>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let details = &self.details;
        let locked = details.locked(attempt);
        let can_edit = details.can_edit();
        let pending = details.pending(attempt);
        let close = cx.listener(|this, _: &ClickEvent, window, cx| this.request_close(window, cx));
        let dialog = if details.confirming_discard(attempt) {
            Dialog::new("edit-channel-details", "Discard changes?")
                .description("Your channel details have unsaved changes.")
                .compact_header_gap()
                .on_close(close)
                .footer(
                    Button::new("keep-editing", "Keep editing")
                        .surface(Surface::Floating)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.details.confirm_discard = false;
                            this.after_step(window, cx);
                        })),
                )
                .footer(
                    Button::new("discard-changes", "Discard changes")
                        .destructive()
                        .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
                )
        } else if let Some(private) = details.privacy_choice {
            let weak = cx.entity().downgrade();
            Dialog::new("edit-channel-details", form_fields::privacy_title(private))
                .compact_header_gap()
                .compact_footer_gap()
                .on_close(close)
                .child(form_fields::privacy_confirmation(
                    private,
                    details.skip_warning,
                    move |checked, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.details.skip_warning = checked;
                            cx.notify();
                        });
                    },
                    cx,
                ))
                .footer(
                    Button::new("privacy-cancel", "Cancel")
                        .surface(Surface::Floating)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.details.privacy_choice = None;
                            this.after_step(window, cx);
                        })),
                )
                .footer(
                    Button::new("privacy-continue", "Continue")
                        .prominent()
                        .disabled(locked || !can_edit)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_privacy(window, cx)),
                        ),
                )
        } else {
            let errors = details.errors(attempt);
            let disabled = locked || !can_edit;
            let current = details.current(attempt).cloned();
            let weak = cx.entity().downgrade();
            let on_duration: form_fields::OnDuration = Rc::new(move |ttl, _, cx| {
                let _ = weak.update(cx, |this, cx| this.set_duration(ttl, cx));
            });
            let form = div()
                .flex()
                .flex_col()
                .gap(space::S4)
                .when_some(current.as_ref(), |this, draft| {
                    this.child(form_fields::name_field(
                        &self.name,
                        errors.name,
                        disabled,
                        cx,
                    ))
                    .child(form_fields::description_field(
                        &self.description,
                        errors.description,
                        disabled,
                        cx,
                    ))
                    .child(form_fields::duration_field(
                        draft.ttl.is_some(),
                        details.temporary_ttl(attempt),
                        disabled,
                        errors.lifetime,
                        on_duration,
                        cx,
                    ))
                })
                .children(self.status(attempt, cx));
            let weak = cx.entity().downgrade();
            let unconfirmed = attempt.is_some_and(|a| a.status == AttemptStatus::Unconfirmed);
            let mut dialog = Dialog::new("edit-channel-details", "Edit channel details")
                .compact_header_gap()
                .on_close(close)
                .child(form);
            if let Some(draft) = &current {
                dialog = dialog.footer_leading(
                    Switch::new("details-private", draft.private)
                        .label("Private")
                        .disabled(disabled)
                        .on_change(move |checked, window, cx| {
                            let _ =
                                weak.update(cx, |this, cx| this.set_private(checked, window, cx));
                        }),
                );
            }
            dialog = dialog.footer(
                Button::new(
                    "details-cancel",
                    if attempt.is_some() { "Close" } else { "Cancel" },
                )
                .surface(Surface::Floating)
                .disabled(pending)
                .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
            );
            if unconfirmed {
                dialog.footer(
                    Button::new("details-check", "Check save status")
                        .prominent()
                        .loading(details.pending)
                        .disabled(details.loading)
                        .on_click(cx.listener(|this, _, window, cx| this.check(window, cx))),
                )
            } else {
                dialog.footer(
                    Button::new("details-save", "Save changes")
                        .prominent()
                        .loading(pending)
                        .disabled(!details.can_save(attempt))
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
            }
        };
        div()
            .track_focus(&self.dialog_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.request_close(window, cx);
                }
            }))
            .child(dialog)
            .into_any_element()
    }
}

impl Render for ChannelSettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let channel = self.channel(cx);
        let attempt = self.attempt(cx);
        let content = channel.as_ref().map(|channel| {
            let editor = editable(channel);
            let edit = (editor && (self.details.can_edit() || attempt.is_some()))
                .then(|| self.details.pending(attempt.as_ref()));
            let name = channel_label(channel, &self.community.read(cx).snapshot.profiles);
            let pending = edit.unwrap_or(false);
            let open = |field: EditField| -> Option<RowClick> {
                edit.map(|_| {
                    let weak = cx.entity().downgrade();
                    Box::new(move |window: &mut Window, cx: &mut App| {
                        let _ = weak.update(cx, |this, cx| this.open_editor(field, window, cx));
                    }) as RowClick
                })
            };
            let mut rows = Vec::new();
            if !matches!(
                channel.channel_type,
                Some(ChannelType::Dm | ChannelType::Session)
            ) {
                let description = match &channel.description {
                    None => "Not available".to_owned(),
                    Some(text) if text.is_empty() => "No description".to_owned(),
                    Some(text) => text.clone(),
                };
                let mut row = Row::new("settings-description", "Description", description);
                row.icon = Some(IconName::PencilSimple);
                row.loading = pending;
                row.on_click = open(EditField::Description);
                rows.push(row);
                let visibility = match channel.visibility {
                    Some(Visibility::Public) => "Public",
                    Some(Visibility::Private) => "Private",
                    None => "Not available",
                };
                let mut row = Row::new("settings-visibility", "Visibility", visibility);
                row.icon = Some(IconName::PencilSimple);
                row.loading = pending;
                row.on_click = open(EditField::Name);
                rows.push(row);
            }
            if !channel.members.is_empty() {
                let mut row = Row::new(
                    "settings-members",
                    "Members",
                    channel.members.len().to_string(),
                );
                row.trailing = Some(IconName::CaretRight);
                if channel.channel_type != Some(ChannelType::Session) {
                    let open_members = self.open_members.clone();
                    row.on_click = Some(Box::new(move |window, cx| open_members(window, cx)));
                }
                rows.push(row);
            }
            let mut row = Row::new("settings-id", "Channel ID", channel.id.clone());
            row.icon = Some(IconName::Copy);
            let weak = cx.entity().downgrade();
            row.on_click = Some(Box::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.copy_id(cx));
            }));
            // A tooltip above the row, over the list and any modal host.
            row.feedback = self.copied.then(|| {
                div()
                    .absolute()
                    .top_0()
                    .left(space::S4)
                    .child(
                        deferred(
                            anchored().anchor(Anchor::BottomLeft).child(
                                div()
                                    .pb(space::S1)
                                    .child(Tooltip::bubble("Channel ID copied", cx)),
                            ),
                        )
                        .with_priority(2),
                    )
                    .into_any_element()
            });
            rows.push(row);
            let last = rows.len() - 1;
            let rows: Vec<Stateful<Div>> = rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    let row = row.render(index == 0, cx);
                    if index != last {
                        return row;
                    }
                    // "Channel ID copied" shows until the pointer leaves.
                    row.on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !hovered && this.copied {
                            this.copied = false;
                            cx.notify();
                        }
                    }))
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .gap(space::S6)
                .child(self.identity(channel, name, edit, cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(c.border_standard)
                        .rounded(radius::PANEL)
                        .children(rows),
                )
                .when(!self.details.editing, |this| {
                    this.children(self.status(attempt.as_ref(), cx))
                })
                .when(shows_lifecycle(channel), |this| {
                    this.child(self.lifecycle_actions(cx))
                })
        });
        let editor = self.details.editing.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |window, cx| {
                let _ = weak.update(cx, |this, cx| this.request_close(window, cx));
            });
            overlay::modal(
                window,
                self.editor_dialog(attempt.as_ref(), cx),
                Some(dismiss),
            )
        });
        let confirm = self
            .confirm
            .as_ref()
            .map(|(dialog, _)| lifecycle_modal(dialog, window));
        div()
            .id("channel-settings")
            .key_context("ChannelSettings")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && !this.details.editing
                    && this.confirm.is_none()
                {
                    cx.stop_propagation();
                    (this.on_close)(window, cx);
                }
            }))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .p(space::S5)
            .text_body_sm()
            .text_color(c.text_standard)
            .children(content)
            .child(self.diagnostics(cx))
            .children(editor)
            .children(confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::EventId;
    use core::prelude::v1::test;

    fn base(can_edit: bool) -> ChannelDetails {
        ChannelDetails {
            channel: "c".into(),
            version: EventId::from_byte_array([0; 32]),
            draft: DetailsDraft {
                name: "general".into(),
                description: "Chat".into(),
                private: false,
                ttl: None,
                labels: None,
            },
            can_edit,
        }
    }

    fn opened() -> Details {
        let mut details = Details::default();
        details.loading();
        details.loaded(base(true));
        assert!(details.edit(EditField::Name, None));
        details
    }

    #[test]
    fn saves_only_dirty_valid_drafts() {
        let mut details = opened();
        assert!(!details.can_save(None), "unchanged");
        details.draft.as_mut().unwrap().name = "  #general ".into();
        assert!(!details.can_save(None), "normalizes to the same name");
        details.draft.as_mut().unwrap().name = "lobby".into();
        assert!(details.can_save(None));
        details.draft.as_mut().unwrap().description = "x".repeat(1001);
        assert!(!details.can_save(None), "invalid");
        details.draft.as_mut().unwrap().description = "Chat".into();
        details.privacy_choice = Some(true);
        assert!(!details.can_save(None), "privacy step open");
        details.privacy_choice = None;
        let saving = DetailsAttempt {
            draft: details.draft.clone().unwrap(),
            status: AttemptStatus::Saving,
        };
        assert!(!details.can_save(Some(&saving)), "locked by the save");
        let mut readonly = Details::default();
        readonly.loaded(base(false));
        assert!(!readonly.edit(EditField::Name, None));
    }

    #[test]
    fn closing_steps_back_through_privacy_and_discard() {
        let mut details = opened();
        details.request_close(None);
        assert!(!details.editing, "nothing typed closes at once");
        let mut details = opened();
        // A trailing space normalizes away but is still typed text.
        details.draft.as_mut().unwrap().name = "general ".into();
        details.request_close(None);
        assert!(details.editing && details.confirm_discard);
        details.request_close(None);
        assert!(
            details.editing && !details.confirm_discard,
            "back to editing"
        );
        details.privacy_choice = Some(true);
        details.request_close(None);
        assert!(details.privacy_choice.is_none() && details.editing);
        details.pending = true;
        details.request_close(None);
        assert!(details.editing, "no close while saving");
    }

    #[test]
    fn an_unconfirmed_save_reopens_with_its_draft() {
        let mut details = Details::default();
        details.loaded(base(true));
        let attempt = DetailsAttempt {
            draft: DetailsDraft {
                name: "lobby".into(),
                ..base(true).draft
            },
            status: AttemptStatus::Unconfirmed,
        };
        assert!(details.edit(EditField::Name, Some(&attempt)));
        assert_eq!(details.draft.as_ref().unwrap().name, "lobby");
        assert!(details.locked(Some(&attempt)));
        assert!(!details.can_save(Some(&attempt)), "check, never resave");
        assert!(!details.unsaved(Some(&attempt)), "closing loses nothing");
    }

    #[test]
    fn reload_keeps_explicit_choices_only() {
        let mut details = opened();
        {
            let draft = details.draft.as_mut().unwrap();
            draft.name = "lobby".into();
            draft.private = true;
        }
        let mut fresh = base(true);
        fresh.draft.ttl = Some(86_400);
        fresh.draft.private = false;
        details.loaded(fresh);
        let draft = details.draft.as_ref().unwrap();
        assert_eq!(draft.name, "lobby");
        assert!(draft.private, "chosen visibility stays");
        assert_eq!(
            draft.ttl,
            Some(86_400),
            "untouched lifetime follows the relay"
        );

        let mut details = opened();
        details.draft.as_mut().unwrap().ttl = Some(1_209_600);
        details.loaded(base(true));
        assert_eq!(details.draft.as_ref().unwrap().ttl, Some(1_209_600));
    }

    #[test]
    fn temporary_duration_defaults_to_a_week() {
        let mut details = opened();
        assert_eq!(details.temporary_ttl(None), DEFAULT_TEMPORARY_TTL);
        details.temporary_ttl = Some(86_400);
        assert_eq!(details.temporary_ttl(None), 86_400);
        details.draft.as_mut().unwrap().ttl = Some(1_209_600);
        assert_eq!(details.temporary_ttl(None), 1_209_600);
    }
}
