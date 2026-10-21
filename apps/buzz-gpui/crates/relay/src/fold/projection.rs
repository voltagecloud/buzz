//! Incremental folding for one open window (`message-projection.ts`,
//! `row-identity.ts`, §5b.3).

use super::{
    FoldOptions, LocalDelivery, MessageClock, MessageRow, compare_messages, e_targets, event_ms,
    fold_rows, is_row_kind,
};
use crate::event::Verified;
use nostr::prelude::{EventId, PublicKey};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// How far a changed overlay walks up to its row: a deletion of a reaction
/// changes the reaction's message, not a row with the deletion's target id.
const PARENT_OVERLAY_DEPTH: usize = 2;
/// How far a row's fold reaches down its overlays: the reactions and edits
/// on it, and the deletions of those.
const CHILD_OVERLAY_DEPTH: usize = 1;

/// Per-window indexes, so new evidence refolds only the rows it touches and
/// delivery-status changes never refold.
///
/// Each [`reconcile`](Self::reconcile) takes the window's whole current
/// event set (fetched pages, live traffic and local outbox events). Rows
/// whose folded value did not change keep their `Arc`, and an unchanged
/// snapshot keeps its `Arc` too, so views can skip re-rendering with
/// `Arc::ptr_eq`.
pub struct MessageProjection {
    channel: Arc<str>,
    relay_author: PublicKey,
    options: FoldOptions,
    clock: Arc<MessageClock>,
    inputs: HashMap<EventId, Verified>,
    /// Target id to the overlays naming it in an `e` tag, in arrival order.
    overlays: HashMap<EventId, Vec<EventId>>,
    messages: HashMap<EventId, Arc<MessageRow>>,
    deliveries: HashMap<EventId, LocalDelivery>,
    rows: Arc<[Arc<MessageRow>]>,
}

impl MessageProjection {
    pub fn new(
        channel: impl Into<Arc<str>>,
        relay_author: PublicKey,
        options: FoldOptions,
    ) -> Self {
        Self {
            channel: channel.into(),
            relay_author,
            options,
            clock: Arc::default(),
            inputs: HashMap::new(),
            overlays: HashMap::new(),
            messages: HashMap::new(),
            deliveries: HashMap::new(),
            rows: Arc::new([]),
        }
    }

    /// Shares the session's send clock, which every new row raises.
    pub fn with_clock(mut self, clock: Arc<MessageClock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn snapshot(&self) -> Arc<[Arc<MessageRow>]> {
        self.rows.clone()
    }

    pub fn options(&self) -> &FoldOptions {
        &self.options
    }

    /// Replaces the options (for example, a new 39001 admin set) and refolds
    /// every row, since any of them may now fold differently.
    pub fn set_options(&mut self, options: FoldOptions) -> (Arc<[Arc<MessageRow>]>, bool) {
        if options == self.options {
            return (self.rows.clone(), false);
        }
        self.options = options;
        let affected: HashSet<EventId> = self
            .inputs
            .values()
            .filter(|event| is_row_kind(event.kind_u16()))
            .map(|event| event.id)
            .collect();
        let mut changed = HashSet::new();
        self.refold(&affected, &mut changed);
        self.publish(changed)
    }

    /// [`reconcile_local`](Self::reconcile_local) with no local deliveries.
    pub fn reconcile<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a Verified>,
    ) -> (Arc<[Arc<MessageRow>]>, bool) {
        self.reconcile_local(events, [])
    }

    /// Folds the window's current events. `deliveries` carries the outbox
    /// state of local events by id. Returns the snapshot and whether it
    /// differs from the previous one.
    pub fn reconcile_local<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a Verified>,
        deliveries: impl IntoIterator<Item = (EventId, LocalDelivery)>,
    ) -> (Arc<[Arc<MessageRow>]>, bool) {
        let mut next: HashMap<EventId, Verified> = HashMap::new();
        let mut added: Vec<&Verified> = Vec::new();
        for event in events {
            if next.insert(event.id, event.clone()).is_none()
                && !self.inputs.contains_key(&event.id)
            {
                added.push(event);
            }
        }
        let deliveries: HashMap<EventId, LocalDelivery> = deliveries.into_iter().collect();

        let mut affected: HashSet<EventId> = HashSet::new();
        for (id, event) in &self.inputs {
            if next.contains_key(id) {
                continue;
            }
            for target in targets(event) {
                affected.insert(target);
                if let Some(refs) = self.overlays.get_mut(&target) {
                    refs.retain(|overlay| overlay != id);
                    if refs.is_empty() {
                        self.overlays.remove(&target);
                    }
                }
            }
        }
        // Event ids name immutable payloads, so a known id needs no refold.
        for event in added {
            let row = is_row_kind(event.kind_u16());
            if row {
                self.clock.observe(&self.channel, event_ms(event));
            }
            for target in targets(event) {
                affected.insert(target);
                if !row {
                    let refs = self.overlays.entry(target).or_default();
                    if !refs.contains(&event.id) {
                        refs.push(event.id);
                    }
                }
            }
        }

        // Walk overlays up to the rows they change. Removed inputs count
        // too, so a failed edit rolls back.
        let mut queue: Vec<(EventId, usize)> = affected.iter().map(|id| (*id, 0)).collect();
        let mut queued = affected.clone();
        let mut index = 0;
        while let Some(&(id, depth)) = queue.get(index) {
            index += 1;
            if depth >= PARENT_OVERLAY_DEPTH {
                continue;
            }
            let Some(event) = next.get(&id).or_else(|| self.inputs.get(&id)) else {
                continue;
            };
            if is_row_kind(event.kind_u16()) {
                continue;
            }
            for parent in e_targets(event) {
                affected.insert(parent);
                if queued.insert(parent) {
                    queue.push((parent, depth + 1));
                }
            }
        }
        self.inputs = next;

        let mut changed = HashSet::new();
        for (id, item) in &deliveries {
            if self.deliveries.get(id) != Some(item) && !affected.contains(id) {
                self.patch_delivery(id, Some(item), &mut changed);
            }
        }
        let gone: Vec<EventId> = self
            .deliveries
            .keys()
            .filter(|id| !deliveries.contains_key(id) && !affected.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.patch_delivery(&id, None, &mut changed);
        }
        self.deliveries = deliveries;

        self.refold(&affected, &mut changed);
        self.publish(changed)
    }

    fn refold(&mut self, affected: &HashSet<EventId>, changed: &mut HashSet<EventId>) {
        for id in affected {
            let folded = self
                .inputs
                .get(id)
                .filter(|event| is_row_kind(event.kind_u16()))
                .and_then(|event| {
                    let mut events = vec![event.clone()];
                    events.extend(
                        self.overlay_ids(id)
                            .iter()
                            .filter_map(|overlay| self.inputs.get(overlay).cloned()),
                    );
                    fold_rows(&self.channel, &self.relay_author, &events, &self.options)
                        .into_iter()
                        .next()
                });
            match folded {
                Some(mut row) => {
                    row.delivery = self.deliveries.get(id).cloned();
                    // Row identity: an equal row keeps its `Arc`.
                    if self.messages.get(id).is_none_or(|old| **old != row) {
                        self.messages.insert(*id, Arc::new(row));
                        changed.insert(*id);
                    }
                }
                None => {
                    if self.messages.remove(id).is_some() {
                        changed.insert(*id);
                    }
                }
            }
        }
    }

    /// The row's overlays, then theirs, in arrival order.
    fn overlay_ids(&self, id: &EventId) -> Vec<EventId> {
        let mut ids: Vec<EventId> = self.overlays.get(id).cloned().unwrap_or_default();
        let mut seen: HashSet<EventId> = ids.iter().copied().collect();
        let mut frontier = ids.clone();
        for _ in 0..CHILD_OVERLAY_DEPTH {
            let mut children = Vec::new();
            for overlay in &frontier {
                for child in self.overlays.get(overlay).into_iter().flatten() {
                    if seen.insert(*child) {
                        ids.push(*child);
                        children.push(*child);
                    }
                }
            }
            if children.is_empty() {
                break;
            }
            frontier = children;
        }
        ids
    }

    fn patch_delivery(
        &mut self,
        id: &EventId,
        item: Option<&LocalDelivery>,
        changed: &mut HashSet<EventId>,
    ) {
        if let Some(row) = self.messages.get_mut(id)
            && row.delivery.as_ref() != item
        {
            *row = Arc::new(MessageRow {
                delivery: item.cloned(),
                ..(**row).clone()
            });
            changed.insert(*id);
        }
    }

    /// Replaces changed rows in the sorted snapshot. Unchanged rows keep
    /// their place and `Arc`; the input is nearly sorted, which the stable
    /// sort handles in close to linear time.
    fn publish(&mut self, changed: HashSet<EventId>) -> (Arc<[Arc<MessageRow>]>, bool) {
        if changed.is_empty() {
            return (self.rows.clone(), false);
        }
        let mut rows: Vec<Arc<MessageRow>> = self
            .rows
            .iter()
            .filter(|row| !changed.contains(&row.id))
            .cloned()
            .collect();
        rows.extend(
            changed
                .iter()
                .filter_map(|id| self.messages.get(id).cloned()),
        );
        rows.sort_by(|a, b| compare_messages(a, b));
        self.rows = rows.into();
        (self.rows.clone(), true)
    }
}

/// Rows stand for themselves; anything else for its `e` targets.
fn targets(event: &Verified) -> Vec<EventId> {
    if is_row_kind(event.kind_u16()) {
        vec![event.id]
    } else {
        e_targets(event)
    }
}
