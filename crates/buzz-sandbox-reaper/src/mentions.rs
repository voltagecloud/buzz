//! Launch registered remote workspaces from ordinary, owner-signed mentions.
//! Clients need no control protocol. Agent-authenticated subscriptions retain
//! the relay's channel access checks; credentials never leave the manager.

mod journal;
mod registry;
pub(crate) use registry::Identity;
mod worker;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::manager::Manager;

const MAX_AGENTS: usize = 64;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Stable labels only: upstream errors can contain credentials or message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum Error {
    #[error("registration_invalid")]
    Registration,
    #[error("inbox_storage_failed")]
    Storage,
    #[error("inbox_lease_unavailable")]
    Lease,
    #[error("mention_capacity_exceeded")]
    Capacity,
    #[error("mention_relay_failed")]
    Relay,
    #[error("mention_deadline_exceeded")]
    Deadline,
}

/// Reconcile the bounded set of agent listeners. Removing/changing a registry
/// entry cancels and joins its old worker before starting a replacement.
pub(crate) async fn run(manager: Arc<Manager>, relay: &str, stop: CancellationToken) {
    let permits = Arc::new(Semaphore::new(4));
    let mut workers: BTreeMap<String, (String, JoinHandle<()>)> = BTreeMap::new();
    let holder = uuid::Uuid::new_v4().to_string();
    let activated = nostr::Timestamp::now().as_secs();
    loop {
        let discovered = tokio::select! {
            () = stop.cancelled() => break,
            result = tokio::time::timeout(IO_TIMEOUT, registry::discover(&manager, relay)) => result.unwrap_or(Err(Error::Deadline)),
        };
        match discovered {
            Ok(identities) => {
                let retired: Vec<_> = workers
                    .iter()
                    .filter(|(name, (revision, handle))| {
                        handle.is_finished()
                            || identities
                                .get(*name)
                                .is_none_or(|identity| &identity.revision != revision)
                    })
                    .map(|(name, _)| name.clone())
                    .collect();
                for name in retired {
                    if let Some((_, handle)) = workers.remove(&name) {
                        handle.abort();
                        let _ = handle.await;
                    }
                }
                for (name, identity) in identities {
                    if workers.contains_key(&name) {
                        continue;
                    }
                    let revision = identity.revision.clone();
                    let manager = Arc::clone(&manager);
                    let permits = Arc::clone(&permits);
                    let holder = holder.clone();
                    let handle = tokio::spawn(async move {
                        worker::run(manager, identity, &holder, activated, permits).await;
                    });
                    workers.insert(name, (revision, handle));
                }
            }
            Err(error) => {
                // Fail closed when revocations cannot be observed. Durable
                // pending work and cursors remain available after recovery.
                tracing::warn!(%error, "mention registry unavailable; suspending listeners");
                for (_, (_, handle)) in std::mem::take(&mut workers) {
                    handle.abort();
                    let _ = handle.await;
                }
            }
        }
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(DISCOVERY_INTERVAL) => {},
        }
    }
    for (_, (_, handle)) in workers {
        handle.abort();
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests;
