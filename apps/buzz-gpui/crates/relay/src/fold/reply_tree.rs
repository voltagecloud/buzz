//! Nesting a thread's replies for display (`messages/reply-tree.ts`,
//! §5c.3 "Reply tree (UI)").

use super::MessageRow;
use nostr::prelude::EventId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A forest of the loaded replies. It never claims every parent was loaded:
/// a reply whose parent is missing, is the root, or forms a cycle sits at
/// the top level, so malformed ancestry cannot hide a row.
#[derive(Debug, Clone, Default)]
pub struct ReplyTree {
    /// Replies directly under the root, in input order.
    pub top_level: Vec<Arc<MessageRow>>,
    /// Nested replies by parent id, in input order.
    pub children: HashMap<EventId, Vec<Arc<MessageRow>>>,
    /// Parent of each nested reply.
    pub parents: HashMap<EventId, EventId>,
}

impl ReplyTree {
    pub fn children_of(&self, id: &EventId) -> &[Arc<MessageRow>] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// Nearest parent first.
    pub fn ancestors(&self, id: &EventId) -> Vec<EventId> {
        let mut result = Vec::new();
        let mut cursor = self.parents.get(id);
        while let Some(parent) = cursor {
            result.push(*parent);
            cursor = self.parents.get(parent);
        }
        result
    }
}

/// Builds the tree from rows already in [`super::compare_messages`] order,
/// which siblings keep.
pub fn reply_tree(rows: &[Arc<MessageRow>], root_id: Option<&EventId>) -> ReplyTree {
    let by_id: HashMap<EventId, &Arc<MessageRow>> = rows.iter().map(|row| (row.id, row)).collect();
    let mut tree = ReplyTree::default();
    for row in rows {
        let mut parent = row
            .reply_parent_id
            .filter(|parent| Some(parent) != root_id && by_id.contains_key(parent));
        // Walk the signed ancestry; a loop back to anything seen is a cycle.
        let mut seen = HashSet::from([row.id]);
        let mut cursor = parent;
        while let Some(id) = cursor {
            if !seen.insert(id) {
                parent = None;
                break;
            }
            cursor = by_id.get(&id).and_then(|row| row.reply_parent_id);
        }
        match parent {
            Some(parent) => {
                tree.parents.insert(row.id, parent);
                tree.children.entry(parent).or_default().push(row.clone());
            }
            None => tree.top_level.push(row.clone()),
        }
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::MessageRow;
    use nostr::prelude::Keys;

    fn id(n: u8) -> EventId {
        EventId::from_byte_array([n; 32])
    }

    fn row(n: u8, parent: Option<u8>) -> Arc<MessageRow> {
        Arc::new(MessageRow {
            id: id(n),
            channel: Arc::from("c"),
            kind: 9,
            audience: None,
            author: Keys::generate().public_key(),
            signer: None,
            workflow_owner: None,
            created_at: 1,
            created_at_ms: 1000,
            content: String::new(),
            source_content: None,
            edited: false,
            attachment_seams: Vec::new(),
            attachment_source_id: None,
            attachments: Vec::new(),
            emoji: Vec::new(),
            mentions: Vec::new(),
            mention_references: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            participants: Vec::new(),
            thread_root_id: None,
            reply_parent_id: parent.map(id),
            sent_from_thread: None,
            diff: None,
            membership: None,
            delivery: None,
        })
    }

    fn ids(rows: &[Arc<MessageRow>]) -> Vec<EventId> {
        rows.iter().map(|row| row.id).collect()
    }

    const ROOT: u8 = 0;

    #[test]
    fn groups_by_signed_ancestry_keeping_sibling_order() {
        let (child, parent, sibling) = (row(1, Some(2)), row(2, Some(ROOT)), row(3, Some(ROOT)));
        let tree = reply_tree(&[child, parent, sibling], Some(&id(ROOT)));
        assert_eq!(ids(&tree.top_level), [id(2), id(3)]);
        assert_eq!(ids(tree.children_of(&id(2))), [id(1)]);
        assert_eq!(tree.ancestors(&id(1)), [id(2)]);
    }

    #[test]
    fn keeps_missing_parents_visible_and_reparents_when_history_arrives() {
        let child = row(1, Some(9));
        let tree = reply_tree(std::slice::from_ref(&child), Some(&id(ROOT)));
        assert_eq!(ids(&tree.top_level), [id(1)]);

        let tree = reply_tree(
            &[child, row(9, Some(2)), row(2, Some(ROOT))],
            Some(&id(ROOT)),
        );
        assert_eq!(tree.ancestors(&id(1)), [id(9), id(2)]);
    }

    #[test]
    fn does_not_lose_rows_to_self_references_or_cycles() {
        let rows = [
            row(1, Some(2)),
            row(2, Some(1)),
            row(3, Some(1)),
            row(4, Some(4)),
        ];
        let tree = reply_tree(&rows, Some(&id(ROOT)));
        assert_eq!(ids(&tree.top_level), ids(&rows));
        assert!(tree.ancestors(&id(1)).is_empty());
    }
}
