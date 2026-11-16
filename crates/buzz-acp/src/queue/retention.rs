use super::{EventQueue, MAX_PENDING_PER_CHANNEL};
use nostr::EventId;
use uuid::Uuid;

impl EventQueue {
    /// Whether an event is still queued, cancelled, or withheld for steering.
    pub(crate) fn retains_event(&self, id: EventId) -> bool {
        self.queues
            .values()
            .flatten()
            .any(|item| item.event.id == id)
            || self
                .cancelled_batches
                .values()
                .flatten()
                .any(|item| item.event.id == id)
            || self
                .withheld_native_steer
                .values()
                .flatten()
                .any(|item| item.event.id == id)
    }
    /// Total queued events across every scope belonging to `channel_id`.
    pub(super) fn channel_event_total(&self, channel_id: Uuid) -> usize {
        self.queues
            .iter()
            .filter(|(s, _)| s.channel_id() == channel_id)
            .map(|(_, q)| q.len())
            .sum()
    }

    /// Drop the globally-oldest queued event(s) across a channel's scopes until
    /// its aggregate depth is within [`MAX_PENDING_PER_CHANNEL`]. Preserves
    /// cross-scope FIFO fairness by always evicting the oldest head event.
    pub(super) fn enforce_channel_cap(&mut self, channel_id: Uuid) {
        while self.channel_event_total(channel_id) > MAX_PENDING_PER_CHANNEL {
            // Find the channel's scope whose head event is oldest.
            let victim = self
                .queues
                .iter()
                .filter(|(s, q)| s.channel_id() == channel_id && !q.is_empty())
                .min_by_key(|(_, q)| q.front().unwrap().received_at)
                .map(|(s, _)| s.clone());
            let Some(scope) = victim else { break };
            if let Some(q) = self.queues.get_mut(&scope) {
                q.pop_front();
                if q.is_empty() {
                    self.queues.remove(&scope);
                }
            }
            tracing::warn!(
                channel_id = %channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "aggregate per-channel queue cap reached — dropped oldest event"
            );
        }
    }
}
