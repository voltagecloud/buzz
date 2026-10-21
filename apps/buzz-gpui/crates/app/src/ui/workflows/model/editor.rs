//! Draft checks and command bookkeeping (`editor-model.ts`, and the lock
//! rules of `WorkflowLanding.tsx`). Draft validation shapes the draft; it
//! is not relay authorization or a promise of execution.

use super::yaml::{self, get};
use buzz_relay_client::workflows::{
    Action, DefinitionsSnapshot, Outcome, ViewStatus, WorkflowDefinition, WorkflowOperation,
    protocol,
};
use serde_yaml::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueField {
    Name,
    Text,
    Duration,
    Timeout,
    Url,
    Headers,
}

/// Why a draft cannot be saved, and the field and step it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub message: &'static str,
    pub field: Option<IssueField>,
    pub step: Option<usize>,
}

fn issue(message: &'static str) -> Option<Issue> {
    Some(Issue {
        message,
        field: None,
        step: None,
    })
}

fn step_issue(message: &'static str, field: IssueField, step: usize) -> Option<Issue> {
    Some(Issue {
        message,
        field: Some(field),
        step: Some(step),
    })
}

fn blank(value: Option<&Value>) -> bool {
    !value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

pub fn draft_issue(text: &str) -> Option<Issue> {
    if text.len() > protocol::MAX_YAML_BYTES {
        return issue("The draft is too large (24,000 bytes maximum).");
    }
    let Ok(document) = yaml::parse(text) else {
        return issue("The YAML cannot be parsed. Correct it before saving.");
    };
    let Value::Mapping(data) = document else {
        return issue("The definition must be a YAML object.");
    };
    if blank(get(&data, "name")) {
        return Some(Issue {
            message: "Give this workflow a name.",
            field: Some(IssueField::Name),
            step: None,
        });
    }
    if get(&data, "enabled").is_some_and(|enabled| !enabled.is_bool()) {
        return issue("enabled must be true or false.");
    }
    if !get(&data, "trigger")
        .and_then(Value::as_mapping)
        .and_then(|trigger| get(trigger, "on"))
        .is_some_and(Value::is_string)
    {
        return issue("Choose a trigger.");
    }
    let Some(steps) = get(&data, "steps")
        .and_then(Value::as_sequence)
        .filter(|steps| !steps.is_empty())
    else {
        return issue("Add at least one step.");
    };
    if steps.len() > 100 {
        return issue("Use at most 100 steps.");
    }
    let mut ids = std::collections::HashSet::new();
    for (index, step) in steps.iter().enumerate() {
        let id = step
            .as_mapping()
            .and_then(|step| get(step, "id"))
            .and_then(Value::as_str);
        let Some(step) = step
            .as_mapping()
            .filter(|_| id.is_some_and(|id| protocol::step_id(id) && ids.insert(id.to_string())))
        else {
            return issue("Step IDs must be unique, with 1–64 letters, digits or underscores.");
        };
        let Some(action) = get(step, "action").and_then(Value::as_str) else {
            return issue("Each step needs an action.");
        };
        if let Some(timeout) = get(step, "timeout_secs") {
            let positive = timeout
                .as_i64()
                .is_some_and(|seconds| seconds > 0 && seconds as u64 <= super::duration::SAFE)
                || timeout.as_f64().is_some_and(|seconds| {
                    seconds.fract() == 0.0
                        && seconds > 0.0
                        && seconds <= super::duration::SAFE as f64
                        && timeout.as_i64().is_none()
                        && timeout.as_u64().is_none()
                });
            if !positive {
                return step_issue(
                    "Step timeout must be a positive whole number of seconds (for example, 30s in Form mode), or left blank.",
                    IssueField::Timeout,
                    index,
                );
            }
        }
        match action {
            "send_message" if blank(get(step, "text")) => {
                return step_issue(
                    "Each Send Message step needs message text.",
                    IssueField::Text,
                    index,
                );
            }
            "call_webhook" => {
                // The relay expands templates before checking the destination.
                if blank(get(step, "url")) {
                    return step_issue(
                        "Enter a webhook URL or template. The relay checks destination safety and permission.",
                        IssueField::Url,
                        index,
                    );
                }
                if get(step, "headers")
                    .and_then(Value::as_mapping)
                    .is_some_and(|headers| {
                        headers
                            .keys()
                            .any(|name| yaml::key_text(name).trim().is_empty())
                    })
                {
                    return step_issue(
                        "Give every header a name or remove its row.",
                        IssueField::Headers,
                        index,
                    );
                }
            }
            "delay" if blank(get(step, "duration")) => {
                return step_issue(
                    "Each Delay step needs a duration.",
                    IssueField::Duration,
                    index,
                );
            }
            _ => {}
        }
    }
    None
}

/// Our successful save, read back at its exact revision. Never another
/// editor's head.
pub fn exact_save_readback<'a>(
    operation: &WorkflowOperation,
    definitions: &'a [WorkflowDefinition],
) -> Option<&'a WorkflowDefinition> {
    if operation.action != Action::Save || operation.outcome != Outcome::Succeeded {
        return None;
    }
    definitions
        .iter()
        .find(|row| row.revision == operation.event_id && row.names(&operation.workflow))
}

/// A successful receipt alone does not prove the saved configuration is
/// gone: a complete fresh read must lack it.
pub fn confirmed_deletion(
    operation: &WorkflowOperation,
    snapshot: Option<&DefinitionsSnapshot>,
) -> bool {
    operation.action == Action::Delete
        && operation.outcome == Outcome::Succeeded
        && snapshot.is_some_and(|snapshot| {
            snapshot.status == ViewStatus::Ready
                && !snapshot.data.partial
                && !snapshot
                    .data
                    .items
                    .iter()
                    .any(|row| row.names(&operation.workflow))
        })
}

pub fn deletion_status(
    operation: &WorkflowOperation,
    snapshot: Option<&DefinitionsSnapshot>,
) -> &'static str {
    if operation.outcome == Outcome::Pending {
        return "Deleting…";
    }
    if operation.outcome == Outcome::Rejected {
        return "Couldn't delete this workflow. Review the delivery details.";
    }
    if confirmed_deletion(operation, snapshot) {
        return "Saved workflow deleted. Work already running may continue.";
    }
    match snapshot {
        Some(snapshot) if snapshot.status == ViewStatus::Loading => "Checking deletion…",
        Some(snapshot)
            if snapshot.status == ViewStatus::Ready
                && snapshot
                    .data
                    .items
                    .iter()
                    .any(|row| row.names(&operation.workflow)) =>
        {
            "This workflow is still in saved configuration. Check again before deleting."
        }
        _ => "Couldn't confirm deletion. Check saved configuration before deleting again.",
    }
}

/// A save is resolved once its exact revision, or a newer head, is read.
pub fn save_awaits_readback(save: &WorkflowOperation, definition: &WorkflowDefinition) -> bool {
    save.event_id != definition.revision && definition.created_at <= save.created_at
}

/// Whether this workflow's configuration still has a write in flight or a
/// save awaiting readback. Delivered run requests never lock it, and a
/// newer head resolves a save; the relay's expected-revision check still
/// refuses stale saves.
pub fn operation_locked(operations: &[WorkflowOperation], definition: &WorkflowDefinition) -> bool {
    let mut newest_save = None;
    for operation in operations.iter().rev() {
        if operation.outcome == Outcome::Rejected || !definition.names(&operation.workflow) {
            continue;
        }
        if operation.action == Action::Delete || operation.outcome == Outcome::Pending {
            return true;
        }
        if operation.action == Action::Save && newest_save.is_none() {
            newest_save = Some(operation);
        }
    }
    newest_save.is_some_and(|save| save_awaits_readback(save, definition))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::Delivery;
    use buzz_relay_client::workflows::{WorkflowDefinitions, WorkflowReference};
    use core::prelude::v1::test;

    const VALID: &str = "name: x\ntrigger: {on: message_posted}\nsteps:\n  - id: a\n    action: send_message\n    text: hi\n";

    #[test]
    fn reports_the_first_draft_problem() {
        assert_eq!(draft_issue(VALID), None);
        let message = |text: &str| draft_issue(text).map(|issue| issue.message);
        assert_eq!(
            message(&"#".repeat(24_001)),
            Some("The draft is too large (24,000 bytes maximum).")
        );
        assert_eq!(
            message("name: ["),
            Some("The YAML cannot be parsed. Correct it before saving.")
        );
        assert_eq!(
            message("- a"),
            Some("The definition must be a YAML object.")
        );
        assert_eq!(
            draft_issue("name: ' '\n").unwrap().field,
            Some(IssueField::Name)
        );
        assert_eq!(
            message("name: x\nenabled: 1\n"),
            Some("enabled must be true or false.")
        );
        assert_eq!(message("name: x\n"), Some("Choose a trigger."));
        assert_eq!(
            message("name: x\ntrigger: {on: webhook}\nsteps: []\n"),
            Some("Add at least one step.")
        );
        assert_eq!(
            message("name: x\ntrigger: {on: webhook}\nsteps: [{id: a-b, action: delay}]\n"),
            Some("Step IDs must be unique, with 1–64 letters, digits or underscores.")
        );
        let text = draft_issue("name: x\ntrigger: {on: webhook}\nsteps: [{id: a, action: delay, duration: 1m}, {id: b, action: send_message, text: ' '}]\n").unwrap();
        assert_eq!((text.field, text.step), (Some(IssueField::Text), Some(1)));
        let timeout = draft_issue("name: x\ntrigger: {on: webhook}\nsteps: [{id: a, action: delay, duration: 1m, timeout_secs: oops}]\n").unwrap();
        assert_eq!(timeout.field, Some(IssueField::Timeout));
        let url = draft_issue(
            "name: x\ntrigger: {on: webhook}\nsteps: [{id: a, action: call_webhook, url: ''}]\n",
        )
        .unwrap();
        assert_eq!(url.field, Some(IssueField::Url));
        let headers = draft_issue("name: x\ntrigger: {on: webhook}\nsteps: [{id: a, action: call_webhook, url: x, headers: {' ': v}}]\n").unwrap();
        assert_eq!(headers.field, Some(IssueField::Headers));
        let delay =
            draft_issue("name: x\ntrigger: {on: webhook}\nsteps: [{id: a, action: delay}]\n")
                .unwrap();
        assert_eq!(delay.field, Some(IssueField::Duration));
    }

    fn definition(revision: &str, created_at: u64) -> WorkflowDefinition {
        WorkflowDefinition {
            id: "55555555-5555-4555-8555-555555555555".into(),
            owner: "11".repeat(32),
            channel_id: "44444444-4444-4444-8444-444444444444".into(),
            revision: revision.into(),
            created_at,
            yaml: VALID.into(),
        }
    }

    fn operation(
        action: Action,
        outcome: Outcome,
        event: &str,
        created_at: u64,
    ) -> WorkflowOperation {
        let row = definition("x", 0);
        WorkflowOperation {
            event_id: event.into(),
            workflow: WorkflowReference {
                id: row.id,
                owner: row.owner,
                channel_id: row.channel_id,
            },
            action,
            created_at,
            delivery: Delivery::Accepted,
            outcome,
            error: None,
            run_id: None,
            secret_held: false,
        }
    }

    #[test]
    fn locks_until_the_save_is_read_back() {
        let head = definition("aa", 100);
        let save = operation(Action::Save, Outcome::Unknown, "bb", 200);
        assert!(operation_locked(std::slice::from_ref(&save), &head));
        // The exact revision, or a newer head, resolves it.
        assert!(!operation_locked(
            std::slice::from_ref(&save),
            &definition("bb", 200)
        ));
        assert!(!operation_locked(
            std::slice::from_ref(&save),
            &definition("cc", 300)
        ));
        // A delivered run request never locks; a pending one does.
        let run = operation(Action::Trigger, Outcome::Unknown, "dd", 300);
        assert!(!operation_locked(&[run], &head));
        let pending = operation(Action::Trigger, Outcome::Pending, "dd", 300);
        assert!(operation_locked(&[pending], &head));
        let delete = operation(Action::Delete, Outcome::Unknown, "ee", 300);
        assert!(operation_locked(&[delete], &head));
        let rejected = operation(Action::Delete, Outcome::Rejected, "ee", 300);
        assert!(!operation_locked(&[rejected], &head));
    }

    #[test]
    fn describes_deletion_from_a_fresh_read() {
        let deleted = operation(Action::Delete, Outcome::Succeeded, "ee", 300);
        let snapshot = |status, items: Vec<WorkflowDefinition>| DefinitionsSnapshot {
            status,
            data: WorkflowDefinitions {
                items,
                ..Default::default()
            },
            error: None,
        };
        let gone = snapshot(ViewStatus::Ready, Vec::new());
        assert!(confirmed_deletion(&deleted, Some(&gone)));
        assert_eq!(
            deletion_status(&deleted, Some(&gone)),
            "Saved workflow deleted. Work already running may continue."
        );
        let still = snapshot(ViewStatus::Ready, vec![definition("aa", 1)]);
        assert_eq!(
            deletion_status(&deleted, Some(&still)),
            "This workflow is still in saved configuration. Check again before deleting."
        );
        assert_eq!(
            deletion_status(&deleted, Some(&snapshot(ViewStatus::Loading, Vec::new()))),
            "Checking deletion…"
        );
        assert_eq!(
            deletion_status(&deleted, None),
            "Couldn't confirm deletion. Check saved configuration before deleting again."
        );
        let pending = operation(Action::Delete, Outcome::Pending, "ee", 300);
        assert_eq!(deletion_status(&pending, Some(&gone)), "Deleting…");
    }

    #[test]
    fn readback_needs_the_exact_revision() {
        let save = operation(Action::Save, Outcome::Succeeded, "bb", 200);
        let rows = [definition("aa", 100), definition("bb", 200)];
        assert_eq!(exact_save_readback(&save, &rows).unwrap().revision, "bb");
        assert!(exact_save_readback(&save, &rows[..1]).is_none());
    }
}
