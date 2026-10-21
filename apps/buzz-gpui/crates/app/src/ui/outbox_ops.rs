//! The outbox's pending and failed operations as message rows read them:
//! edits and deletions in flight lock their row's menu items, reactions in
//! flight lock its reaction controls, and failed ones offer a retry.

use buzz_relay_client::fold::MessageRow;
use buzz_relay_client::{Delivery, EventId, Operation, Outbox, kinds};
use std::collections::HashMap;
use std::sync::Arc;

/// One operation, reduced to what rows ask about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    pub id: EventId,
    pub kind: u16,
    /// The first `e` tag.
    pub target: Option<EventId>,
    pub channel: Option<String>,
    /// The first `k` tag, on deletions.
    pub target_kind: Option<String>,
    pub content: String,
    pub delivery: Delivery,
    pub error: Option<String>,
}

impl Op {
    pub fn from_operation(op: &Operation) -> Self {
        Self {
            id: op.id(),
            kind: op.kind(),
            target: op.tag("e").and_then(|id| EventId::from_hex(id).ok()),
            channel: op.channel().map(str::to_string),
            target_kind: op.tag("k").map(str::to_string),
            content: op.event.content.clone(),
            delivery: op.delivery,
            error: op.error.clone(),
        }
    }

    fn failed(&self) -> bool {
        matches!(self.delivery, Delivery::Failed | Delivery::Unknown)
    }
}

/// A snapshot of the outbox, plus which reaction each deletion removes.
#[derive(Clone, Debug, Default)]
pub struct Ops {
    pub list: Vec<Op>,
    /// Reaction id → the message it reacts to, for deletions of reactions,
    /// whose own tags name only the reaction.
    pub reaction_parents: HashMap<EventId, EventId>,
}

impl Ops {
    pub fn read(outbox: &Outbox, reaction_parents: HashMap<EventId, EventId>) -> Self {
        Self {
            list: outbox
                .visible()
                .iter()
                .map(|op| Op::from_operation(op))
                .collect(),
            reaction_parents,
        }
    }

    fn targets(&self, op: &Op, id: EventId) -> bool {
        op.target == Some(id)
    }

    /// An edit or deletion of `id` is still sending or awaiting its echo.
    pub fn changing(&self, id: EventId) -> bool {
        self.list.iter().any(|op| {
            matches!(op.delivery, Delivery::Sending | Delivery::Accepted)
                && matches!(op.kind, kinds::DELETION | kinds::EDIT)
                && self.targets(op, id)
        })
    }

    /// Any deletion of `id` in the outbox.
    pub fn deleting(&self, id: EventId) -> bool {
        self.list
            .iter()
            .any(|op| op.kind == kinds::DELETION && self.targets(op, id))
    }

    /// Any edit of `id` in the outbox.
    pub fn editing(&self, id: EventId) -> bool {
        self.list
            .iter()
            .any(|op| op.kind == kinds::EDIT && self.targets(op, id))
    }

    /// The deletion of `id`, for the delete dialog.
    pub fn deletion(&self, id: EventId) -> Option<&Op> {
        self.list
            .iter()
            .find(|op| op.kind == kinds::DELETION && self.targets(op, id))
    }

    pub fn get(&self, id: EventId) -> Option<&Op> {
        self.list.iter().find(|op| op.id == id)
    }

    /// `reactionTarget`: the message a reaction or a reaction's deletion
    /// is about.
    pub fn reaction_target(&self, op: &Op, row: Option<&MessageRow>) -> Option<EventId> {
        match op.kind {
            kinds::REACTION => op.target,
            kinds::DELETION => {
                let target = op.target?;
                if let Some(parent) = self.reaction_parents.get(&target) {
                    return Some(*parent);
                }
                row.filter(|row| {
                    row.reactions
                        .iter()
                        .any(|group| group.events.iter().any(|r| r.id == target))
                })
                .map(|row| row.id)
            }
            _ => None,
        }
    }

    /// A reaction change for `row` that blocks another toggle: sending,
    /// failed or unknown.
    pub fn reaction_blocked(&self, row: &MessageRow) -> bool {
        self.list.iter().any(|op| {
            matches!(
                op.delivery,
                Delivery::Sending | Delivery::Failed | Delivery::Unknown
            ) && self.reaction_target(op, Some(row)) == Some(row.id)
        })
    }

    /// A failed or unconfirmed reaction change for `row`.
    pub fn reaction_failure(&self, row: &MessageRow) -> Option<&Op> {
        self.list
            .iter()
            .find(|op| op.failed() && self.reaction_target(op, Some(row)) == Some(row.id))
    }

    /// Failed or unconfirmed edits and message deletions in `channel`
    /// (`MessageManagementStatus`).
    pub fn recovery(&self, channel: &str) -> Vec<&Op> {
        self.list
            .iter()
            .filter(|op| {
                (op.kind == kinds::EDIT
                    || (op.kind == kinds::DELETION
                        && matches!(op.target_kind.as_deref(), Some("9" | "40002"))))
                    && op.channel.as_deref() == Some(channel)
                    && op.failed()
            })
            .collect()
    }
}

pub type SharedOps = Arc<Ops>;

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn op(id: u8, kind: u16, target: u8, delivery: Delivery) -> Op {
        Op {
            id: EventId::from_byte_array([id; 32]),
            kind,
            target: Some(EventId::from_byte_array([target; 32])),
            channel: Some("c".into()),
            target_kind: Some("9".into()),
            content: String::new(),
            delivery,
            error: None,
        }
    }

    #[test]
    fn locks_follow_the_outbox() {
        let message = EventId::from_byte_array([1; 32]);
        let ops = Ops {
            list: vec![
                op(9, kinds::EDIT, 1, Delivery::Sending),
                op(8, kinds::DELETION, 2, Delivery::Failed),
            ],
            reaction_parents: HashMap::new(),
        };
        assert!(ops.changing(message));
        assert!(ops.editing(message));
        assert!(!ops.deleting(message));
        assert_eq!(ops.recovery("c").len(), 1);
        assert!(ops.deletion(EventId::from_byte_array([2; 32])).is_some());
    }

    #[test]
    fn reaction_deletions_map_to_their_message() {
        let mut row = super::super::markdown::tests::row("hi");
        row.id = EventId::from_byte_array([1; 32]);
        let mut parents = HashMap::new();
        parents.insert(EventId::from_byte_array([5; 32]), row.id);
        let ops = Ops {
            list: vec![op(7, kinds::DELETION, 5, Delivery::Unknown)],
            reaction_parents: parents,
        };
        assert!(ops.reaction_blocked(&row));
        assert!(ops.reaction_failure(&row).is_some());
        let react = Ops {
            list: vec![op(6, kinds::REACTION, 1, Delivery::Seen)],
            reaction_parents: HashMap::new(),
        };
        assert!(!react.reaction_blocked(&row));
    }
}
