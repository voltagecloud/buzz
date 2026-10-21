//! Channel activity for the sidebar's "recent" order (`channel-activity.ts`
//! and its session wiring): while some section sorts by recency, the
//! newest message time of every joined channel, read in batches of 128
//! and advanced by live traffic.

use super::{Actor, Message, Session};
use crate::event::{Verified, Verifier};
use crate::http::Priority;
use crate::sidebar::{ACTIVITY_BATCH, ActivityRefresh, ChannelActivity, activity_filters};
use std::time::Duration;

/// Each batch read gives up after this.
const BATCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Activity {
    /// Some section sorts by recency.
    pub wanted: bool,
    pub values: ChannelActivity,
    /// The roster the values were last refreshed for.
    roster: Option<Vec<String>>,
    run: Option<Run>,
    generation: u64,
}

struct Run {
    ids: Vec<String>,
    next: usize,
    refresh: ActivityRefresh,
}

impl Session {
    /// Reads `ChannelSummary::last_activity_at` while some sidebar section
    /// sorts by recent activity; live traffic keeps it current either way.
    pub fn track_channel_activity(&self, wanted: bool) {
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.activity.wanted = wanted;
            // Off stops the reads; what live traffic told stays (`cancel`).
            if !wanted {
                actor.activity_cancel();
            } else {
                actor.refresh_activity();
            }
        })));
    }
}

impl Actor {
    /// Reads activity again when the roster changed (`refreshChannelActivity`).
    pub(super) fn refresh_activity(&mut self) {
        if !self.activity.wanted || self.relay.is_none() {
            return;
        }
        let Some(discovery) = &self.discovery else {
            return;
        };
        let mut ids: Vec<String> = discovery
            .channels()
            .into_iter()
            .filter(|channel| !channel.cached)
            .map(|channel| channel.id)
            .collect();
        ids.sort();
        if self.activity.roster.as_ref() == Some(&ids) {
            return;
        }
        self.activity.roster = Some(ids.clone());
        self.activity.generation += 1;
        if ids.is_empty() {
            self.activity.values.commit(&[], ActivityRefresh::default());
            self.publish();
            return;
        }
        self.activity.run = Some(Run {
            refresh: self.activity.values.begin_refresh(),
            ids,
            next: 0,
        });
        self.activity_next();
    }

    fn activity_next(&mut self) {
        let Some(run) = &mut self.activity.run else {
            return;
        };
        let batch: Vec<String> = run
            .ids
            .iter()
            .skip(run.next)
            .take(ACTIVITY_BATCH)
            .cloned()
            .collect();
        if batch.is_empty() {
            let run = self.activity.run.take().expect("running");
            self.activity.values.commit(&run.ids, run.refresh);
            self.publish();
            return;
        }
        run.next += batch.len();
        let Ok(filters) = activity_filters(&batch) else {
            self.activity_failed();
            return;
        };
        let (http, verifier, commands) = (
            self.http.clone(),
            self.verifier.clone(),
            self.commands.clone(),
        );
        let generation = self.activity.generation;
        self.runtime.spawn(async move {
            let read = async {
                let raw = http
                    .query_with(&filters, Priority::Background)
                    .await
                    .map_err(|error| error.to_string())?;
                let verifier: &Verifier = &verifier;
                verifier.verify_all(&raw).map_err(|error| error.to_string())
            };
            let result = tokio::time::timeout(BATCH_TIMEOUT, read)
                .await
                .unwrap_or_else(|_| Err("Channel activity read timed out".into()));
            let _ = commands.send(Message::With(Box::new(move |actor| {
                actor.activity_read(generation, batch, result);
            })));
        });
    }

    fn activity_read(
        &mut self,
        generation: u64,
        batch: Vec<String>,
        result: Result<Vec<Verified>, String>,
    ) {
        if generation != self.activity.generation {
            return;
        }
        match result {
            Ok(events) => {
                if let Some(run) = &mut self.activity.run {
                    run.refresh.collect(&batch, &events);
                }
                self.activity_next();
            }
            Err(error) => {
                tracing::debug!(%error, "channel activity read failed");
                self.activity_failed();
            }
        }
    }

    /// A failed pass leaves the values and tries again with the next
    /// roster change or reconnect.
    fn activity_failed(&mut self) {
        self.activity.run = None;
        self.activity.roster = None;
    }

    /// The stream dropped or access changed: the pass in flight is
    /// abandoned and the next establishment reads again.
    pub(super) fn activity_cancel(&mut self) {
        self.activity.generation += 1;
        self.activity.run = None;
        self.activity.roster = None;
    }

    /// Live traffic only advances values, whatever the sort
    /// (`channelActivity.accept`).
    pub(super) fn activity_accept(&mut self, events: &[Verified]) -> bool {
        self.activity.values.accept(events)
    }
}
