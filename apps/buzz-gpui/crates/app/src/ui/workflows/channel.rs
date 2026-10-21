//! One channel's workflows (`WorkflowChannel.tsx`): its saved
//! configurations, the draft in the editor, and the commands for it.
//! Opened from a landing card it is only the editor ("detail").

use super::controls::{self, Confirm, Handler};
use super::editor::{self, ActionItem, Editor, EditorHost, Props};
use super::model::editor::{exact_save_readback, save_awaits_readback};
use super::model::form::new_draft_yaml;
use super::model::yaml as document;
use super::operations::{self, OperationsHost, OperationsState};
use super::runs::RunsView;
use crate::mirror::mirror;
use buzz_relay_client::workflows::{
    Action, Definitions, DefinitionsSnapshot, Outcome, ViewStatus, WorkflowDefinition,
    WorkflowOperation, Workflows,
};
use buzz_ui::{ActiveTheme, Button, Icon, IconName, Surface, TypeStyled, tokens::space};
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// What the channel opens: a saved definition or a new draft.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    Definition(WorkflowDefinition),
    New,
}

#[derive(Clone, Debug, PartialEq)]
enum Pending {
    Open(Selection),
    Close,
}

struct Draft {
    original: Option<WorkflowDefinition>,
    yaml: String,
    initial: String,
    operation: Option<String>,
}

pub enum ChannelEvent {
    /// Detail closed: back to the landing.
    Close,
    /// Deletion confirmed; the page submits it and owns recovery.
    Delete(WorkflowDefinition),
    /// The exact revision of our save was read back.
    SaveReadback { channel: String, revision: String },
}

impl EventEmitter<ChannelEvent> for WorkflowChannel {}

pub struct WorkflowChannel {
    workflows: Workflows,
    channel_id: String,
    channel_name: String,
    viewer: String,
    detail_only: bool,
    view: Option<Definitions>,
    view_error: Option<String>,
    snapshot: Arc<DefinitionsSnapshot>,
    operations: Arc<[WorkflowOperation]>,
    draft: Option<Draft>,
    editor: Option<Editor>,
    generation: u64,
    /// A command being submitted, or awaiting its outcome.
    submission: Option<String>,
    pending: Option<Pending>,
    confirm_delete: bool,
    confirm_run: bool,
    error: Option<String>,
    read_runs: bool,
    created_run: Option<String>,
    runs: Option<(String, Entity<RunsView>)>,
    local_risk: bool,
    refreshed: Option<String>,
    details_open: bool,
    ops: OperationsState,
    confirm_focus: FocusHandle,
    _watch: Vec<Task<()>>,
}

impl WorkflowChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workflows: Workflows,
        channel_id: String,
        channel_name: String,
        viewer: String,
        initial: Option<Selection>,
        run: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let operations = workflows.operations();
        let mut this = Self {
            detail_only: initial.is_some(),
            view: None,
            view_error: None,
            snapshot: Arc::new(DefinitionsSnapshot {
                status: ViewStatus::Idle,
                data: Default::default(),
                error: None,
            }),
            operations,
            draft: None,
            editor: None,
            generation: 0,
            submission: None,
            pending: None,
            confirm_delete: false,
            confirm_run: run,
            error: None,
            read_runs: false,
            created_run: None,
            runs: None,
            local_risk: false,
            refreshed: None,
            details_open: false,
            ops: OperationsState::new(cx),
            confirm_focus: cx.focus_handle(),
            _watch: Vec::new(),
            workflows,
            channel_id,
            channel_name,
            viewer,
        };
        match this
            .workflows
            .definitions(vec![this.channel_id.clone()], false)
        {
            Ok(view) => {
                // Read first, so the view is never seen idle before it starts.
                drop(view.refresh(false));
                this.snapshot = view.snapshot();
                this._watch
                    .push(mirror(view.watch(), cx, |this: &mut Self, value| {
                        this.snapshot = value;
                    }));
                this.view = Some(view);
            }
            Err(error) => this.view_error = Some(error),
        }
        this._watch.push(mirror(
            this.workflows.watch_operations(),
            cx,
            |this: &mut Self, value| this.operations = value,
        ));
        if let Some(initial) = initial {
            this.open_draft(initial, cx);
        }
        this
    }

    fn refresh(&self) {
        if let Some(view) = &self.view {
            drop(view.refresh(false));
        }
    }

    fn open_draft(&mut self, next: Selection, cx: &mut Context<Self>) {
        let (original, yaml) = match next {
            Selection::New => (None, new_draft_yaml()),
            Selection::Definition(definition) => {
                let yaml = definition.yaml.clone();
                (Some(definition), yaml)
            }
        };
        self.generation += 1;
        self.editor = Some(Editor::new(&yaml, original.is_none(), false, cx));
        self.draft = Some(Draft {
            original,
            initial: yaml.clone(),
            yaml,
            operation: None,
        });
    }

    /// `open`: explicit replacement discards local form state.
    fn open(&mut self, next: Pending, cx: &mut Context<Self>) {
        if next == Pending::Close && self.detail_only {
            cx.emit(ChannelEvent::Close);
            return;
        }
        self.local_risk = false;
        match next {
            Pending::Close => {
                self.draft = None;
                self.editor = None;
            }
            Pending::Open(selection) => self.open_draft(selection, cx),
        }
        self.submission = None;
        self.error = None;
        self.read_runs = false;
        self.runs = None;
        self.pending = None;
        self.confirm_delete = false;
        cx.notify();
    }

    fn at_risk(&self) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|d| d.yaml != d.initial || self.local_risk || d.operation.is_some())
    }

    fn dirty(&self) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|d| d.yaml != d.initial || self.local_risk)
    }

    fn select(&mut self, next: Pending, cx: &mut Context<Self>) {
        if self.at_risk() {
            self.pending = Some(next);
            cx.notify();
        } else {
            self.open(next, cx);
        }
    }

    fn own_operations(&self) -> Vec<WorkflowOperation> {
        self.operations
            .iter()
            .filter(|op| op.workflow.channel_id == self.channel_id)
            .cloned()
            .collect()
    }

    fn draft_operation(&self) -> Option<&WorkflowOperation> {
        let id = self.draft.as_ref()?.operation.as_ref()?;
        self.operations.iter().find(|op| &op.event_id == id)
    }

    fn busy(&self) -> bool {
        self.draft.as_ref().is_some_and(|d| d.operation.is_some())
            && self
                .draft_operation()
                .is_none_or(|op| op.outcome == Outcome::Pending)
    }

    fn readonly(&self) -> bool {
        self.draft
            .as_ref()
            .and_then(|d| d.original.as_ref())
            .is_some_and(|original| original.owner != self.viewer)
    }

    /// A configuration write for this draft that is unresolved. A run
    /// request never leaves one, and a newer head resolves a save.
    fn unresolved_write(&self) -> bool {
        let original = self.draft.as_ref().and_then(|d| d.original.as_ref());
        self.own_operations().iter().any(|item| match original {
            None => {
                item.action == Action::Save
                    && !matches!(item.outcome, Outcome::Rejected | Outcome::Succeeded)
            }
            Some(original) => {
                if item.workflow.id != original.id || item.workflow.owner != original.owner {
                    return false;
                }
                if item.outcome == Outcome::Pending {
                    return true;
                }
                if item.action == Action::Delete {
                    return item.outcome != Outcome::Rejected;
                }
                item.action == Action::Save
                    && item.outcome == Outcome::Unknown
                    && save_awaits_readback(item, original)
            }
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let availability = self.workflows.availability();
        let Some(draft) = &self.draft else { return };
        if self.submission.is_some()
            || self.busy()
            || self.unresolved_write()
            || self.readonly()
            || !availability.save
            || draft.operation.is_some()
        {
            return;
        }
        match self.workflows.save(
            &self.channel_id,
            draft.yaml.clone(),
            draft.original.as_ref(),
        ) {
            Ok(id) => {
                self.submission = Some(id.clone());
                if let Some(draft) = &mut self.draft {
                    draft.operation = Some(id);
                }
                self.error = None;
            }
            Err(error) => {
                self.submission = None;
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(definition) = self.draft.as_ref().and_then(|d| d.original.clone()) else {
            return;
        };
        if self.submission.is_some()
            || self.readonly()
            || self.unresolved_write()
            || !self.workflows.availability().delete
            || self.draft.as_ref().is_some_and(|d| d.operation.is_some())
        {
            return;
        }
        self.draft = None;
        self.editor = None;
        self.local_risk = false;
        self.pending = None;
        self.confirm_delete = false;
        cx.emit(ChannelEvent::Delete(definition));
        cx.notify();
    }

    fn trigger(&mut self, cx: &mut Context<Self>) {
        let Some(definition) = self.draft.as_ref().and_then(|d| d.original.clone()) else {
            return;
        };
        if self.submission.is_some()
            || self.dirty()
            || self.unresolved_write()
            || self.draft.as_ref().is_some_and(|d| d.operation.is_some())
            || self.readonly()
            || !self.workflows.availability().trigger
        {
            return;
        }
        match self.workflows.trigger(&definition) {
            Ok(id) => {
                self.submission = Some(id);
                self.error = None;
            }
            Err(error) => {
                self.submission = None;
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    /// The effects of new data (`useEffect`s): refresh after a successful
    /// save, adopt its exact readback, withdraw a cleared view, and show a
    /// created run.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if let Some(operation) = self.draft_operation().cloned() {
            let key = format!("{}:{}", operation.event_id, operation.outcome.as_str());
            if operation.outcome == Outcome::Succeeded && self.refreshed.as_ref() != Some(&key) {
                self.refreshed = Some(key);
                self.refresh();
            }
            if self.snapshot.status == ViewStatus::Ready
                && let Some(saved) = exact_save_readback(&operation, &self.snapshot.data.items)
            {
                let saved = saved.clone();
                self.submission = None;
                cx.emit(ChannelEvent::SaveReadback {
                    channel: saved.channel_id.clone(),
                    revision: saved.revision.clone(),
                });
                // Readback updates the draft, not its editor.
                self.draft = Some(Draft {
                    yaml: saved.yaml.clone(),
                    initial: saved.yaml.clone(),
                    original: Some(saved),
                    operation: None,
                });
            }
        }
        if matches!(
            self.snapshot.status,
            ViewStatus::Unavailable | ViewStatus::Idle
        ) && self.view.is_some()
            && (self.draft.is_some() || self.pending.is_some() || self.read_runs)
        {
            self.submission = None;
            self.draft = None;
            self.editor = None;
            self.pending = None;
            self.confirm_delete = false;
            self.read_runs = false;
            self.error = None;
        }
        if let Some(active) = self
            .submission
            .as_ref()
            .and_then(|id| self.operations.iter().find(|op| &op.event_id == id))
            .cloned()
            && active.action == Action::Trigger
            && active.outcome != Outcome::Pending
        {
            self.submission = None;
            // Like base Buzz, a created run is shown, not just acknowledged.
            if let Some(run) = active.run_id {
                self.created_run = Some(run);
                self.read_runs = true;
            }
        }
    }

    fn blocked(&self) -> Option<SharedString> {
        let has_operation = self.draft.as_ref().is_some_and(|d| d.operation.is_some());
        if !self.workflows.availability().save {
            Some("Saving is unavailable from this host.".into())
        } else if self.unresolved_write() && !has_operation {
            Some("Check saved configuration or review Recent activity before continuing.".into())
        } else if has_operation {
            match self.draft_operation().map(|op| op.outcome) {
                Some(Outcome::Succeeded) => Some("Configuration saved; waiting for a readback of this exact revision. Check saved configuration or review the current version in Recent activity.".into()),
                Some(Outcome::Rejected) => Some("Draft retained. Review the error below before continuing.".into()),
                _ => None,
            }
        } else {
            None
        }
    }

    fn runs_view(&mut self, cx: &mut Context<Self>) -> Option<Entity<RunsView>> {
        let original = self.draft.as_ref()?.original.clone()?;
        if !self.read_runs {
            self.runs = None;
            return None;
        }
        let key = format!(
            "{}:{}:{}",
            original.owner,
            original.id,
            self.created_run.clone().unwrap_or_default()
        );
        if self
            .runs
            .as_ref()
            .is_none_or(|(existing, _)| existing != &key)
        {
            let workflows = self.workflows.clone();
            let view = cx.new(|cx| RunsView::new(workflows, original, cx));
            self.runs = Some((key, view));
        }
        self.runs.as_ref().map(|(_, view)| view.clone())
    }

    fn handler(
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Handler {
        let host = cx.entity().downgrade();
        let f = Rc::new(f);
        Rc::new(move |_, cx| {
            let f = f.clone();
            let _ = host.update(cx, move |this, cx| {
                f(this, cx);
                cx.notify();
            });
        })
    }

    fn editor_props(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Props {
        let c = *cx.colors();
        let availability = self.workflows.availability();
        let readonly = self.readonly();
        let dirty = self.dirty();
        let unresolved = self.unresolved_write();
        let draft = self.draft.as_ref().expect("draft");
        let has_operation = draft.operation.is_some();
        let original = draft.original.clone();
        let operation = self.draft_operation().cloned();
        let read_runs = self.read_runs;
        let mut actions = vec![ActionItem::Item {
            id: "workflow-read-runs",
            label: if read_runs { "Hide runs" } else { "Read runs" }.into(),
            icon: if read_runs {
                IconName::EyeSlash
            } else {
                IconName::ListBullets
            },
            danger: false,
            disabled: !availability.history,
            run: Self::handler(cx, |this, _| this.read_runs = !this.read_runs),
        }];
        if readonly {
            actions.push(ActionItem::Enable);
        } else {
            actions.push(ActionItem::Item {
                id: "workflow-run-now",
                label: "Run now".into(),
                icon: IconName::Play,
                danger: false,
                disabled: dirty || has_operation || unresolved || !availability.trigger,
                run: Self::handler(cx, |this, cx| this.trigger(cx)),
            });
            actions.push(ActionItem::Enable);
            actions.push(ActionItem::Separator);
            actions.push(ActionItem::Item {
                id: "workflow-delete",
                label: "Delete workflow".into(),
                icon: IconName::Trash,
                danger: true,
                disabled: has_operation || unresolved || !availability.delete,
                run: Self::handler(cx, |this, _| this.confirm_delete = true),
            });
        }
        let mut status: Vec<AnyElement> = Vec::new();
        let snapshot = self.snapshot.clone();
        if matches!(snapshot.status, ViewStatus::Error | ViewStatus::Loading) {
            status.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S4)
                    .pt(space::S4)
                    .child(if snapshot.status == ViewStatus::Error {
                        controls::danger(
                            snapshot.error.clone().unwrap_or_else(|| {
                                "Configurations could not be read. Refresh to retry.".into()
                            }),
                            cx,
                        )
                    } else {
                        div().child("Reading configurations…")
                    })
                    .child(
                        div().flex().child(
                            Button::new(
                                "workflow-refresh-configurations",
                                "Refresh configurations",
                            )
                            .surface(Surface::Floating)
                            .disabled(snapshot.status == ViewStatus::Loading)
                            .on_click(cx.listener(|this, _, _, _| this.refresh())),
                        ),
                    )
                    .into_any_element(),
            );
        }
        if !availability.delete {
            status.push(controls::note("You can't delete workflows here.", cx).into_any_element());
        }
        if let Some(error) = &self.error {
            status.push(controls::danger(error.clone(), cx).into_any_element());
        }
        if operation
            .as_ref()
            .is_some_and(|op| op.outcome == Outcome::Rejected)
        {
            status.push(
                div()
                    .flex()
                    .child(
                        Button::new("workflow-continue-draft", "Continue editing retained draft")
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submission = None;
                                if let Some(draft) = &mut this.draft {
                                    draft.operation = None;
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if let Some(runs) = self.runs_view(cx) {
            status.push(runs.into_any_element());
        }
        let own: Vec<WorkflowOperation> = self
            .own_operations()
            .into_iter()
            .filter(|op| op.action != Action::Delete)
            .collect();
        if let Some(section) = operations::render(self, &own, &snapshot, window, cx) {
            status.push(section);
        }
        let mut details: Vec<AnyElement> = vec![
            controls::note(
                "Configuration changes apply on save. Turning off does not confirm runs have stopped or cancel active runs.",
                cx,
            )
            .into_any_element(),
            controls::note(
                "Unsaved changes stay in this editor. Leaving or reloading discards them without cancelling submitted operations.",
                cx,
            )
            .into_any_element(),
        ];
        if let Some(original) = &original {
            let open = self.ops_details_open();
            details.push(
                controls::disclosure(
                    "workflow-configuration-details",
                    "Configuration details",
                    open,
                    cx.listener(|this, _, _, cx| {
                        this.details_open = !this.details_open;
                        cx.notify();
                    }),
                    cx,
                )
                .into_any_element(),
            );
            if open {
                details.push(
                    div()
                        .text_mono()
                        .child(format!("Owner: {}", original.owner))
                        .into_any_element(),
                );
                details.push(
                    div()
                        .text_mono()
                        .child(format!("Revision: {}", original.revision))
                        .into_any_element(),
                );
            }
        }
        let mut children: Vec<AnyElement> = Vec::new();
        if let Some(pending) = self.pending.clone() {
            let cancel = Self::handler(cx, |this, _| this.pending = None);
            let host = cx.entity().downgrade();
            children.push(
                Confirm::new(
                    "Leave this draft?",
                    "Unsaved draft changes will be discarded. Any submitted operation stays with the captured community session; leaving does not cancel or repeat it.",
                    "Leave draft",
                )
                .render(
                    "workflow-leave",
                    &self.confirm_focus,
                    Rc::new(move |_, cx| {
                        let pending = pending.clone();
                        let _ = host.update(cx, |this, cx| this.open(pending, cx));
                    }),
                    cancel,
                    window,
                    cx,
                ),
            );
        } else if self.confirm_run {
            children.push(
                Confirm::new(
                    "Run this workflow now?",
                    "This submits a manual run of the saved configuration, not unsaved edits.",
                    "Run now",
                )
                .render(
                    "workflow-run",
                    &self.confirm_focus,
                    Self::handler(cx, |this, cx| {
                        this.confirm_run = false;
                        this.trigger(cx);
                    }),
                    Self::handler(cx, |this, _| this.confirm_run = false),
                    window,
                    cx,
                ),
            );
        } else if self.confirm_delete {
            children.push(
                Confirm::new(
                    "Delete this workflow?",
                    "The saved workflow may remain visible. Work already running may continue.",
                    "Delete workflow",
                )
                .cancel("Cancel")
                .destructive()
                .error(self.error.clone().map(SharedString::from))
                .render(
                    "workflow-delete",
                    &self.confirm_focus,
                    Self::handler(cx, |this, cx| this.remove(cx)),
                    Self::handler(cx, |this, _| this.confirm_delete = false),
                    window,
                    cx,
                ),
            );
        }
        let busy = self.busy();
        let blocked = self.blocked();
        let draft = self.draft.as_ref().expect("draft");
        Props {
            read_only: readonly,
            blocked,
            busy,
            locked: draft.operation.is_some(),
            create: draft.original.is_none(),
            choose_channel: false,
            initial_yaml: draft.original.as_ref().map(|o| o.yaml.clone()),
            scope: Some(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .text_label()
                    .text_color(c.text_standard)
                    .child(Icon::new(IconName::Hash).size(18.))
                    .child(self.channel_name.clone())
                    .into_any_element(),
            ),
            actions: Some(actions),
            status,
            details,
            children,
        }
    }

    fn ops_details_open(&self) -> bool {
        self.details_open
    }
}

impl EditorHost for WorkflowChannel {
    fn editor(&mut self) -> Option<&mut Editor> {
        self.editor.as_mut()
    }

    fn editor_ref(&self) -> Option<&Editor> {
        self.editor.as_ref()
    }

    fn draft_yaml(&self) -> String {
        self.draft
            .as_ref()
            .map(|d| d.yaml.clone())
            .unwrap_or_default()
    }

    fn draft_changed(&mut self, yaml: String, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.draft {
            draft.yaml = yaml;
        }
        cx.notify();
    }

    fn save_draft(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    fn cancel_draft(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.select(Pending::Close, cx);
    }

    fn local_draft_risk(&mut self, at_risk: bool, cx: &mut Context<Self>) {
        self.local_risk = at_risk;
        cx.notify();
    }
}

impl OperationsHost for WorkflowChannel {
    fn operations_state(&mut self) -> &mut OperationsState {
        &mut self.ops
    }

    fn operations_state_ref(&self) -> &OperationsState {
        &self.ops
    }

    fn capability(&self) -> &Workflows {
        &self.workflows
    }

    fn check_saved(&mut self, _: &mut Context<Self>) {
        self.refresh();
    }

    fn review_saved(
        &mut self,
        definition: WorkflowDefinition,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // In detail-only mode this replaces the one workflow shown.
        self.select(Pending::Open(Selection::Definition(definition)), cx);
    }

    fn dismissed(&mut self, event_id: &str, _: &mut Context<Self>) {
        if self.submission.as_deref() == Some(event_id) {
            self.submission = None;
        }
        if let Some(draft) = &mut self.draft
            && draft.operation.as_deref() == Some(event_id)
        {
            draft.operation = None;
        }
    }
}

impl Render for WorkflowChannel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let c = *cx.colors();
        let availability = self.workflows.availability();
        let snapshot = self.snapshot.clone();
        let mut section = div()
            .flex()
            .flex_col()
            .text_body()
            .text_color(c.text_standard);
        if let Some(error) = &self.view_error {
            return section
                .child(controls::danger(error.clone(), cx))
                .into_any_element();
        }
        if !self.detail_only {
            let unavailable = matches!(snapshot.status, ViewStatus::Unavailable | ViewStatus::Idle);
            section = section
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(space::S3)
                        .my(space::S4)
                        .child(div().text_heading().child("Saved configurations"))
                        .child(
                            Button::new("workflow-channel-refresh", "Refresh configurations")
                                .disabled(snapshot.status == ViewStatus::Loading)
                                .on_click(cx.listener(|this, _, _, _| this.refresh())),
                        )
                        .child(
                            Button::new("workflow-channel-new", "New workflow")
                                .disabled(!availability.save || unavailable)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.select(Pending::Open(Selection::New), cx)
                                })),
                        ),
                )
                .child(controls::note(
                    "Saved configuration only. Turning off does not confirm runs have stopped or cancel active runs.",
                    cx,
                ));
        }
        let mut states = div().flex().flex_col().gap(space::S2).my(space::S2);
        if snapshot.status == ViewStatus::Loading && snapshot.data.items.is_empty() {
            states = states.child(div().child("Reading configurations…"));
        }
        if snapshot.status == ViewStatus::Idle {
            states = states.child(div().child("Configurations cleared. Refresh to read again."));
        }
        if snapshot.status == ViewStatus::Unavailable {
            states = states.child(div().child("Workflow definitions are unavailable."));
        }
        if snapshot.status == ViewStatus::Error {
            states = states.child(controls::danger(
                snapshot.error.clone().unwrap_or_else(|| {
                    "Configurations could not be read. Use Refresh configurations to retry.".into()
                }),
                cx,
            ));
        }
        if snapshot.data.partial {
            states = states.child(
                div()
                    .text_color(c.text_subtle)
                    .child("This is a bounded, partial list."),
            );
        }
        if !availability.save {
            states = states.child(controls::note(
                "Creating and saving workflows is unavailable from this host. You can browse saved configurations, but cannot save changes here yet.",
                cx,
            ));
        }
        if !self.detail_only
            && snapshot.status == ViewStatus::Ready
            && snapshot.data.items.is_empty()
        {
            states = states.child(div().child(format!(
                "No saved configurations returned for this channel.{}",
                if availability.save {
                    " Create a disabled draft to start."
                } else {
                    ""
                }
            )));
        }
        section = section.child(states);
        if !self.detail_only {
            let mut list = div().flex().flex_col().my(space::S4);
            for definition in &snapshot.data.items {
                let header = document::read_fields(&definition.yaml);
                let state = if header.editable {
                    if header.enabled == Some(false) {
                        "Configured disabled"
                    } else {
                        "Configured enabled"
                    }
                } else {
                    "Unreadable configuration"
                };
                let readonly = if definition.owner != self.viewer {
                    " · Read-only"
                } else {
                    ""
                };
                let open = definition.clone();
                list = list.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(space::S4)
                        .py(space::S2)
                        .border_b_1()
                        .border_color(c.border_standard)
                        .child(
                            Button::new(
                                SharedString::from(format!("workflow-open-{}", definition.id)),
                                header
                                    .name
                                    .clone()
                                    .unwrap_or_else(|| "Unnamed or malformed workflow".into()),
                            )
                            .ghost()
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.select(
                                        Pending::Open(Selection::Definition(open.clone())),
                                        cx,
                                    )
                                },
                            )),
                        )
                        .child(controls::note(format!("{state}{readonly}"), cx)),
                );
            }
            section = section.child(list);
        }
        let dialog = if self.draft.is_some()
            && !matches!(snapshot.status, ViewStatus::Unavailable | ViewStatus::Idle)
        {
            let props = self.editor_props(window, cx);
            Some(editor::render(self, props, window, cx))
        } else {
            None
        };
        if self.draft.is_none() {
            let own: Vec<WorkflowOperation> = self
                .own_operations()
                .into_iter()
                .filter(|op| op.action != Action::Delete)
                .collect();
            if let Some(operations) = operations::render(self, &own, &snapshot, window, cx) {
                section = section.child(operations);
            }
        }
        section.children(dialog).into_any_element()
    }
}
