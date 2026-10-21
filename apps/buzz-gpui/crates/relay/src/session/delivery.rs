//! Waiting for an outbox operation to be delivered. Each caller keeps its
//! own wording and decides what a vanished or uncertain operation means.

use crate::outbox::{Delivery, Outbox};
use nostr::prelude::EventId;

/// How [`delivered`] ends when the operation does not arrive.
pub(super) struct Wait<'a> {
    /// The error when the operation leaves the outbox first; `None`
    /// counts that as delivered.
    pub missing: Option<&'a str>,
    /// The error of a failed operation that carries none of its own.
    pub failed: &'a str,
    /// Whether an uncertain delivery fails the wait too, rather than
    /// waiting on for the outbox to settle it.
    pub unknown_fails: bool,
    /// The error when the outbox closes.
    pub closed: &'a str,
}

/// Waits until the outbox shows `id` accepted or seen. Callers bound the
/// wait themselves.
pub(super) async fn delivered(outbox: &Outbox, id: EventId, wait: &Wait<'_>) -> Result<(), String> {
    let mut changes = outbox.changes();
    loop {
        changes.borrow_and_update();
        let Some(op) = outbox.get(&id) else {
            return wait.missing.map_or(Ok(()), |error| Err(error.to_owned()));
        };
        let failed = match op.delivery {
            Delivery::Accepted | Delivery::Seen => return Ok(()),
            Delivery::Failed => true,
            Delivery::Unknown => wait.unknown_fails,
            Delivery::Sending => false,
        };
        if failed {
            return Err(op.error.clone().unwrap_or_else(|| wait.failed.to_owned()));
        }
        if changes.changed().await.is_err() {
            return Err(wait.closed.to_owned());
        }
    }
}
