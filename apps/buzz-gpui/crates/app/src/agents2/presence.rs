//! Says which agents this copy of the app runs are online (owner presence,
//! kind 20003; buzz-app's `announce` in `service.ts`). Agents run here have
//! no socket of their own, so without it they always read as offline, and
//! an owner could not tell from another device whether the computer
//! running them is awake. The relay keeps it for 180 seconds.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

/// How often agents are said to be online.
pub(crate) const EVERY: Duration = Duration::from_secs(60);
/// The most agents one presence event names.
pub(crate) const BATCH: usize = 256;
/// How soon a refused send is tried again: sooner than the minute, but not
/// every tick.
const RETRY: Duration = Duration::from_secs(30);

/// What was last said to a community.
struct Sent {
    /// The session it went on, by identity.
    session: usize,
    online: BTreeSet<String>,
    at: Instant,
    /// The session cannot send presence, so it is not tried again.
    unable: bool,
    id: u64,
}

/// One community's announcement: agents now offline, then those online.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Announcement {
    pub origin: String,
    pub id: u64,
    pub offline: Vec<String>,
    pub online: Vec<String>,
}

/// What each community was told, by origin.
#[derive(Default)]
pub(crate) struct Announcer {
    sent: HashMap<String, Sent>,
    /// Origins whose presence is being sent.
    announcing: HashSet<String>,
    next: u64,
}

impl Announcer {
    /// What to tell each bound community now (`bound`: its origin, and its
    /// session's identity once bound), given the agents this copy runs
    /// there: online every minute while it does, on a new session, and
    /// offline once it stops, unless `yielded` says another copy took the
    /// agent over. Offline is best effort; the relay lets a lapsed online
    /// expire.
    pub(crate) fn plan(
        &mut self,
        bound: &[(String, Option<usize>)],
        online: &HashMap<String, BTreeSet<String>>,
        yielded: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Vec<Announcement> {
        self.sent
            .retain(|origin, _| bound.iter().any(|(bound, _)| bound == origin));
        let mut announcements = Vec::new();
        for (origin, session) in bound {
            let Some(session) = *session else {
                continue;
            };
            if self.announcing.contains(origin) {
                continue;
            }
            let next = online.get(origin).cloned().unwrap_or_default();
            let prior = self.sent.get_mut(origin);
            let same = prior.as_ref().is_some_and(|prior| prior.session == session);
            if same && prior.as_ref().is_some_and(|prior| prior.unable) {
                continue;
            }
            // Another copy that took an agent over says it is online;
            // offline from this one could arrive after that, so forget it
            // was online here.
            let was: BTreeSet<String> = prior
                .as_ref()
                .map(|prior| {
                    prior
                        .online
                        .iter()
                        .filter(|pubkey| next.contains(*pubkey) || !yielded(pubkey))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let due = match &prior {
                Some(prior) if same => now.duration_since(prior.at) >= EVERY,
                _ => true,
            };
            if let Some(prior) = prior {
                prior.online = was.clone();
            }
            let gone: Vec<String> = was.difference(&next).cloned().collect();
            let changed = !gone.is_empty() || next.iter().any(|pubkey| !was.contains(pubkey));
            if !changed && (!due || next.is_empty()) {
                continue;
            }
            self.next += 1;
            self.announcing.insert(origin.clone());
            self.sent.insert(
                origin.clone(),
                Sent {
                    session,
                    online: next.clone(),
                    at: now,
                    unable: false,
                    id: self.next,
                },
            );
            announcements.push(Announcement {
                origin: origin.clone(),
                id: self.next,
                offline: gone,
                online: next.into_iter().collect(),
            });
        }
        announcements
    }

    /// Notes how announcement `id` went: sent, refused for good by its
    /// session (`Ok(false)`), or failed, to try again in a while.
    pub(crate) fn finished(&mut self, origin: &str, id: u64, outcome: Result<bool, String>) {
        self.announcing.remove(origin);
        let Some(sent) = self.sent.get_mut(origin).filter(|sent| sent.id == id) else {
            return;
        };
        match outcome {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("agent presence cannot be sent on this connection");
                sent.unable = true;
            }
            Err(error) => {
                tracing::warn!(%error, "agent presence was not sent");
                sent.at = Instant::now() - EVERY + RETRY;
            }
        }
    }
}

#[cfg(test)]
mod tests;
