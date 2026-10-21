//! "Recent activity" (`WorkflowOperations.tsx`): what happened to the
//! commands this session sent, with recovery and a dismissal that neither
//! undoes nor repeats anything.

use super::controls::{self, Confirm};
use super::model::editor::{confirmed_deletion, deletion_status};
use crate::runtime::Runtime;
use buzz_relay_client::workflows::{
    Action, DefinitionsSnapshot, Outcome, ViewStatus, WorkflowDefinition, WorkflowOperation,
    Workflows, delivery_label,
};
use buzz_ui::{ActiveTheme, Button, ButtonSize, Surface, TypeStyled, tokens::space};
use gpui_kit::*;
use std::collections::HashSet;
use std::rc::Rc;

/// The section's own state: which notice is being dismissed, and which
/// show their delivery details.
pub struct OperationsState {
    acknowledge: Option<WorkflowOperation>,
    working: bool,
    error: Option<String>,
    open: HashSet<String>,
    focus: FocusHandle,
}

impl OperationsState {
    pub fn new(cx: &mut App) -> Self {
        Self {
            acknowledge: None,
            working: false,
            error: None,
            open: HashSet::new(),
            focus: cx.focus_handle(),
        }
    }
}

pub trait OperationsHost: Sized + 'static {
    fn operations_state(&mut self) -> &mut OperationsState;
    fn operations_state_ref(&self) -> &OperationsState;
    fn capability(&self) -> &Workflows;
    /// "Check saved configuration": a fresh read of the channel.
    fn check_saved(&mut self, cx: &mut Context<Self>);
    /// "Review current configuration".
    fn review_saved(
        &mut self,
        definition: WorkflowDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    /// After a notice's durable dismissal.
    fn dismissed(&mut self, _event_id: &str, _cx: &mut Context<Self>) {}
}

fn message(operation: &WorkflowOperation) -> &'static str {
    match (operation.action, operation.outcome) {
        (Action::Save, Outcome::Pending) => "Saving configuration…",
        (Action::Save, Outcome::Succeeded) => "Configuration saved.",
        (Action::Save, Outcome::Rejected) => "Configuration was not saved.",
        (Action::Save, Outcome::Unknown) => {
            "Save response was lost. Check the saved configuration before trying again."
        }
        (Action::Trigger, Outcome::Pending) => "Requesting a run…",
        (Action::Trigger, Outcome::Succeeded) => {
            "Run requested. Inspect run history for its result."
        }
        (Action::Trigger, Outcome::Rejected) => "Failed to trigger workflow",
        (Action::Trigger, Outcome::Unknown) => {
            "The run may have started, but its response was lost. Checking configuration or dismissing this notice cannot confirm a run."
        }
        (Action::Delete, _) => "",
    }
}

/// Dismisses `operation` once the outbox has durably removed it.
fn dismiss<V: OperationsHost>(this: &mut V, cx: &mut Context<V>) {
    let state = this.operations_state();
    let Some(operation) = state.acknowledge.clone() else {
        return;
    };
    if state.working {
        return;
    }
    state.working = true;
    let work = Runtime::handle(cx).spawn(this.capability().dismiss(&operation.event_id));
    cx.spawn(async move |host, cx| {
        let result = work
            .await
            .unwrap_or_else(|_| Err("The request could not be completed. Try again.".into()));
        let _ = host.update(cx, |this, cx| {
            let state = this.operations_state();
            state.working = false;
            match result {
                Ok(()) => {
                    state.error = None;
                    state.acknowledge = None;
                    this.dismissed(&operation.event_id, cx);
                }
                Err(error) => state.error = Some(error),
            }
            cx.notify();
        });
    })
    .detach();
    cx.notify();
}

/// The section, or nothing when there is no activity.
pub fn render<V: OperationsHost>(
    this: &mut V,
    operations: &[WorkflowOperation],
    snapshot: &DefinitionsSnapshot,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    let state = this.operations_state_ref();
    if operations.is_empty() && state.acknowledge.is_none() {
        return None;
    }
    let c = *cx.colors();
    let working = state.working;
    let acknowledge = state.acknowledge.clone();
    let error = state.error.clone();
    let open = state.open.clone();
    let focus = state.focus.clone();
    let mut section = div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .py(space::S4)
        .border_t_1()
        .border_color(c.border_standard)
        .child(div().text_heading().child("Recent activity"));
    if let (Some(error), None) = (&error, &acknowledge) {
        section = section.child(div().child(error.clone()));
    }
    let items: &[_] = if snapshot.status == ViewStatus::Ready {
        &snapshot.data.items
    } else {
        &[]
    };
    for operation in operations {
        let current = items
            .iter()
            .find(|definition| definition.names(&operation.workflow))
            .cloned();
        let status = if operation.action == Action::Delete {
            deletion_status(operation, Some(snapshot))
        } else {
            message(operation)
        };
        let mut row = div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .child(div().child(status));
        if operation.action == Action::Trigger && operation.outcome == Outcome::Rejected {
            row = row.child(controls::danger(
                operation
                    .error
                    .clone()
                    .unwrap_or_else(|| "The relay did not create a workflow run.".into()),
                cx,
            ));
        } else if operation.action != Action::Delete
            && let Some(error) = &operation.error
        {
            row = row.child(controls::danger(error.clone(), cx));
        }
        let recoverable = matches!(operation.action, Action::Save | Action::Delete)
            && matches!(operation.outcome, Outcome::Unknown | Outcome::Succeeded)
            && !confirmed_deletion(operation, Some(snapshot));
        let id = operation.event_id.clone();
        let mut toolbar = div().flex().flex_wrap().items_center().gap(space::S3);
        if recoverable {
            toolbar = toolbar.child(
                Button::new(
                    SharedString::from(format!("op-check-{id}")),
                    "Check saved configuration",
                )
                .size(ButtonSize::Sm)
                .surface(Surface::Floating)
                .disabled(working || snapshot.status == ViewStatus::Loading)
                .on_click(cx.listener(|this: &mut V, _, _, cx| this.check_saved(cx))),
            );
            if let Some(current) = current.filter(|current| current.revision != operation.event_id)
            {
                toolbar = toolbar.child(
                    Button::new(
                        SharedString::from(format!("op-review-{id}")),
                        "Review current configuration",
                    )
                    .size(ButtonSize::Sm)
                    .surface(Surface::Floating)
                    .disabled(working)
                    .on_click(cx.listener(
                        move |this: &mut V, _, window, cx| {
                            this.review_saved(current.clone(), window, cx)
                        },
                    )),
                );
            }
        }
        if operation.outcome != Outcome::Pending {
            let operation = operation.clone();
            toolbar = toolbar.child(
                Button::new(
                    SharedString::from(format!("op-dismiss-{id}")),
                    "Dismiss notice",
                )
                .size(ButtonSize::Sm)
                .surface(Surface::Floating)
                .disabled(working)
                .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                    this.operations_state().acknowledge = Some(operation.clone());
                    cx.notify();
                })),
            );
        }
        row = row.child(toolbar);
        let details_open = open.contains(&id);
        let toggle_id = id.clone();
        row = row.child(controls::disclosure(
            SharedString::from(format!("op-details-{id}")),
            "Delivery details",
            details_open,
            cx.listener(move |this: &mut V, _, _, cx| {
                let state = this.operations_state();
                if !state.open.remove(&toggle_id) {
                    state.open.insert(toggle_id.clone());
                }
                cx.notify();
            }),
            cx,
        ));
        if details_open {
            if operation.action == Action::Delete
                && let Some(error) = &operation.error
            {
                row = row.child(controls::danger(error.clone(), cx));
            }
            row = row
                .child(div().text_body_sm().child(format!(
                    "{}: {} · delivery {}",
                    operation.action.as_str(),
                    operation.outcome.as_str(),
                    delivery_label(operation.delivery)
                )))
                .child(div().text_mono().child(format!("Event ID: {id}")))
                .children(
                    operation
                        .run_id
                        .as_ref()
                        .map(|run| div().text_mono().child(format!("Returned run ID: {run}"))),
                );
        }
        section = section.child(row);
    }
    if let Some(acknowledge) = acknowledge {
        let host = cx.entity().downgrade();
        let cancel = host.clone();
        section = section.child(
            Confirm::new(
                "Dismiss this notice?",
                if acknowledge.action == Action::Delete {
                    "Your draft is kept and editing is unlocked. Dismissing this notice does not confirm, cancel, or repeat deletion. Check saved configuration before deleting again."
                } else {
                    "Dismissal only clears this notice and its editor lock. It does not undo, cancel, or repeat a command, or confirm its outcome. Review the saved configuration before saving again; a new run request may run the workflow again. Your unsaved draft is kept."
                },
                if working {
                    "Dismissing…"
                } else {
                    "Dismiss notice and continue"
                },
            )
            .pending(working)
            .error(error.map(SharedString::from))
            .render(
                "op-dismiss",
                &focus,
                Rc::new(move |_, cx| {
                    let _ = host.update(cx, |this, cx| dismiss(this, cx));
                }),
                Rc::new(move |_, cx| {
                    let _ = cancel.update(cx, |this, cx| {
                        let state = this.operations_state();
                        if !state.working {
                            state.acknowledge = None;
                        }
                        cx.notify();
                    });
                }),
                window,
                cx,
            ),
        );
    }
    Some(section.into_any_element())
}
