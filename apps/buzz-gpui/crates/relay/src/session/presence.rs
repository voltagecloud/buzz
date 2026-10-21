//! Presence and typing inside the session actor (spec §5h).
//!
//! [`PresenceDirectory`], [`PresencePublisher`] and [`Typing`] are
//! synchronous; this module runs the snapshot reads and publications they
//! ask for and feeds them live events.
//!
//! [`PresenceDirectory`]: crate::presence::PresenceDirectory
//! [`PresencePublisher`]: crate::presence::PresencePublisher
//! [`Typing`]: crate::typing::Typing

use super::unread::now_ms;
use super::{Actor, Message};
use crate::event::Verified;
use crate::http::HttpError;
use crate::presence::{Publication, PublishOutcome, SnapshotRead, SnapshotResult, parse_snapshot};
use crate::socket::PresenceSend;
use buzz_identity::Purpose;
use std::sync::Arc;
use std::time::Duration;

/// A presence snapshot read or publication gives up after this.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest presence snapshot accepted.
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

impl Actor {
    /// The next wall-clock time [`Self::presence_tick`] has work.
    pub(super) fn presence_deadline(&self) -> Option<u64> {
        [
            self.presence.deadline(),
            self.publisher.deadline(),
            self.typing.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(super) fn presence_tick(&mut self) {
        let now = now_ms();
        if let Some(read) = self.presence.due(now) {
            self.read_presence(read);
        }
        if let Some(publication) = self.publisher.due(now) {
            self.publish_presence(publication);
        }
        self.typing.tick(now);
        self.publish_activity();
    }

    /// Publishes the session snapshot if presence or typing changed.
    pub(super) fn publish_activity(&mut self) {
        let presence = self.presence.snapshot();
        let changed = {
            let current = self.snapshot.borrow();
            !Arc::ptr_eq(&current.presence, &presence)
                || !Arc::ptr_eq(&current.typing, &self.typing.snapshot().channels)
                || !Arc::ptr_eq(&current.thread_typing, &self.typing.snapshot().threads)
        };
        if changed {
            self.publish();
        }
    }

    pub(super) fn presence_connected(&mut self, connected: bool) {
        let now = now_ms();
        self.presence.set_connected(connected, now);
        self.publisher.set_connected(connected, now);
        if !connected {
            self.typing.clear();
        }
        self.publish_activity();
    }

    pub(super) fn presence_visible(&mut self, visible: bool) {
        self.presence.set_visible(visible, now_ms());
        if !visible && let Some(live) = &mut self.live {
            let _ = live.watch_presence(Vec::new());
        }
        self.publish_activity();
    }

    pub(super) fn watch_presence(&mut self, authors: Vec<String>) {
        self.presence.set_demand(authors, now_ms());
        if self.presence.demand().is_empty()
            && let Some(live) = &mut self.live
        {
            let _ = live.watch_presence(Vec::new());
        }
        self.publish_activity();
    }

    pub(super) fn presence_live(&mut self, events: &[Verified]) {
        let now = now_ms();
        for event in events {
            self.presence.live(event, now);
        }
        self.publish_activity();
    }

    /// Typing pulses and the messages that end them, from live channel
    /// routes.
    pub(super) fn typing_accept(&mut self, events: &[Verified]) {
        let discovery = &self.discovery;
        let can_access = |id: &str| discovery.as_ref().is_some_and(|d| d.can_access(id));
        if self.typing.accept(events, can_access, now_ms()) {
            self.publish_activity();
        }
    }

    /// `POST /query` for relay-signed statuses. The live route follows the
    /// same authors, updated here so it changes at the read cadence rather
    /// than with every demand change.
    fn read_presence(&mut self, read: SnapshotRead) {
        let Some(relay) = self.relay.as_ref().map(|relay| relay.authority.clone()) else {
            self.presence
                .finish(read, SnapshotResult::Failed { retry_ms: None }, now_ms());
            return;
        };
        if let Some(live) = &mut self.live
            && let Err(error) = live.watch_presence(read.authors.clone())
        {
            tracing::warn!(%error, "presence authors refused");
        }
        let (http, verifier, commands) = (
            self.http.clone(),
            self.verifier.clone(),
            self.commands.clone(),
        );
        self.runtime.spawn(async move {
            let response = tokio::time::timeout(PRESENCE_TIMEOUT, http.post_presence(read.body()))
                .await
                .unwrap_or(Err(HttpError::Transport("Presence read timed out".into())));
            let result = match response {
                Ok(bytes) if bytes.len() <= MAX_SNAPSHOT_BYTES => {
                    match serde_json::from_slice(&bytes)
                        .map_err(|error| error.to_string())
                        .and_then(|raw| parse_snapshot(&raw, &read.authors, &relay, &verifier))
                    {
                        Ok(values) => SnapshotResult::Statuses(values),
                        Err(error) => {
                            tracing::debug!(%error, "presence snapshot rejected");
                            SnapshotResult::Failed { retry_ms: None }
                        }
                    }
                }
                Ok(_) => SnapshotResult::Failed { retry_ms: None },
                Err(HttpError::Status { retry_after, .. }) => SnapshotResult::Failed {
                    retry_ms: retry_after.map(|after| after.as_millis() as u64),
                },
                Err(HttpError::Paused { retry_after }) => SnapshotResult::Failed {
                    retry_ms: Some(retry_after.as_millis() as u64),
                },
                Err(_) => SnapshotResult::Failed { retry_ms: None },
            };
            let _ = commands.send(Message::PresenceRead { read, result });
        });
    }

    pub(super) fn presence_read(&mut self, read: SnapshotRead, result: SnapshotResult) {
        self.presence.finish(read, result, now_ms());
        self.publish_activity();
    }

    /// Signs owner presence for the viewer's `agents`, and the socket to
    /// send it on; `None` when this session can never sign it.
    pub(super) fn agent_presence(
        &self,
        status: &str,
        agents: Vec<String>,
    ) -> Result<Option<(crate::socket::SocketPublisher, nostr::prelude::Event)>, String> {
        let Some(socket) = self.live.as_ref().map(|live| live.socket().publisher()) else {
            return Err("Agent presence needs a live connection".into());
        };
        let tags = agents
            .into_iter()
            .map(|agent| vec!["p".to_string(), agent])
            .collect();
        let signed = self
            .identity
            .template(
                Purpose::Presence,
                buzz_identity::AGENT_PRESENCE,
                now_ms() / 1000,
                tags,
                status.to_owned(),
            )
            .and_then(|unsigned| self.identity.sign(Purpose::Presence, unsigned));
        Ok(signed.ok().map(|event| (socket, event)))
    }

    /// Signs our status (bare content, no tags) and sends it over the
    /// authenticated socket only: never HTTP, never the outbox.
    fn publish_presence(&mut self, publication: Publication) {
        let now = now_ms();
        let Some(socket) = self.live.as_ref().map(|live| live.socket().publisher()) else {
            self.publisher
                .finished(publication, PublishOutcome::NotSent, now);
            return;
        };
        let signed = self
            .identity
            .template(
                Purpose::Presence,
                crate::event::kinds::PRESENCE,
                now / 1000,
                Vec::new(),
                publication.state.as_str().to_owned(),
            )
            .and_then(|unsigned| self.identity.sign(Purpose::Presence, unsigned));
        let event = match signed {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "could not sign presence");
                self.publisher
                    .finished(publication, PublishOutcome::NotSent, now);
                return;
            }
        };
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let outcome = match socket.publish_presence(event).await {
                PresenceSend::Accepted => PublishOutcome::Accepted,
                PresenceSend::NotSent => PublishOutcome::NotSent,
                PresenceSend::Cooling(wait) => PublishOutcome::Cooling(wait.as_millis() as u64),
                PresenceSend::Unconfirmed => PublishOutcome::Unconfirmed,
            };
            let _ = commands.send(Message::PresencePublished {
                publication,
                outcome,
            });
        });
    }

    pub(super) fn presence_published(&mut self, publication: Publication, outcome: PublishOutcome) {
        let now = now_ms();
        if let Some(state) = self.publisher.finished(publication, outcome, now) {
            self.presence.accepted(state, now);
        }
        self.publish_activity();
    }
}
