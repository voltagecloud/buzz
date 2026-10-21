//! Message actions beyond send, edit, react and remove (spec §5d.5,
//! §5d.8, §5i): thread follows, a message's own read state, deletions by
//! an agent's owner, and the outbox itself so recovery UIs can list and
//! retry its operations. Reports and "Send to channel" go through the
//! session's write checks (`Session::report`, `Session::send_to_channel`).

use super::util::NOT_CONNECTED;
use super::{Actor, Session};
use crate::event::Verified;
use crate::messages;
use crate::outbox::Outbox;
use nostr::prelude::EventId;

impl Actor {
    /// A message in `channel` Buzz holds: in its timeline, an open thread,
    /// recent traffic or the outbox (`find`: local, recent, retained).
    pub(super) fn find_message(&self, channel: &str, id: &EventId) -> Option<Verified> {
        self.timelines
            .get(channel)
            .and_then(|timeline| timeline.find(id))
            .or_else(|| {
                self.threads
                    .values()
                    .filter(|thread| thread.channel == channel)
                    .find_map(|thread| thread.state.event(id).cloned())
            })
            .or_else(|| {
                self.find_event(id)
                    .filter(|event| event.channel() == Some(channel))
            })
    }
}

impl Session {
    /// Of `ids`, the channels the viewer may read now (`canAccess`):
    /// denied, suspended and unknown ones are not.
    pub async fn accessible_channels(&self, ids: Vec<String>) -> Vec<String> {
        self.ops()
            .with(move |actor| {
                actor.discovery.as_ref().map_or_else(Vec::new, |discovery| {
                    ids.into_iter()
                        .filter(|id| discovery.can_access(id))
                        .collect()
                })
            })
            .await
            .unwrap_or_default()
    }

    /// Takes the channel metadata and rosters (39000, 39002) a read
    /// brought before its access check, as `session.read` accepts its
    /// batch into discovery first (`acceptDiscovery`). Discovery trusts
    /// only the relay's own key; other events here are ignored.
    pub async fn accept_discovery(&self, events: Vec<Verified>) {
        let events: Vec<Verified> = events
            .into_iter()
            .filter(|event| {
                matches!(
                    event.kind_u16(),
                    crate::kinds::CHANNEL_METADATA | crate::kinds::CHANNEL_MEMBERS
                )
            })
            .collect();
        if events.is_empty() {
            return;
        }
        let _ = self
            .with_actor(move |actor| {
                actor.accept(&events);
            })
            .await;
    }

    /// Runs `work` on the actor and hands back its answer.
    async fn with_actor<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Actor) -> R + Send + 'static,
    ) -> Result<R, String> {
        super::util::with(&self.commands, work).await
    }

    /// The session's outbox, once connected. A reconnect replaces it, so
    /// callers fetch it again when the session becomes ready.
    pub async fn outbox(&self) -> Option<Outbox> {
        self.with_actor(|actor| actor.outbox.clone())
            .await
            .ok()
            .flatten()
    }

    /// Reacts with `content`, reusing a reaction pill's own emoji tag, so a
    /// custom emoji works even when the viewer's catalog lacks it.
    pub async fn react_with(
        &self,
        channel: &str,
        id: EventId,
        content: &str,
        emoji: Option<messages::CustomEmoji>,
    ) -> Result<(), String> {
        self.write(super::Write::React {
            channel: channel.to_owned(),
            id,
            content: content.to_owned(),
            emoji,
        })
        .await
    }

    /// Whether the viewer follows the thread rooted at `root`.
    pub async fn following(&self, channel: &str, root: EventId) -> bool {
        let channel = channel.to_owned();
        self.with_actor(move |actor| {
            actor
                .reads
                .as_mut()
                .is_some_and(|reads| reads.unread.following(&channel, &root.to_hex()))
        })
        .await
        .unwrap_or(false)
    }

    /// Follows or unfollows a thread on this device.
    pub async fn follow(
        &self,
        channel: &str,
        root: EventId,
        following: bool,
    ) -> Result<(), String> {
        let channel = channel.to_owned();
        self.with_actor(move |actor| {
            let reads = actor.reads.as_mut().ok_or(NOT_CONNECTED)?;
            let result = reads
                .unread
                .follow(&channel, &root.to_hex(), following)
                .map_err(|error| error.to_string());
            actor.publish();
            result
        })
        .await?
    }

    /// A loaded message's attention under the badges' policy
    /// (`unread.attention`); `None` before read state loads.
    pub async fn message_attention(
        &self,
        channel: &str,
        id: EventId,
    ) -> Option<crate::unread::MessageAttention> {
        let channel = channel.to_owned();
        self.with_actor(move |actor| {
            let reads = actor.reads.as_mut()?;
            Some(reads.unread.attention(&channel, &id.to_hex(), &reads.sync))
        })
        .await
        .ok()
        .flatten()
    }

    /// Whether a loaded message shows as unread (or forced unread) now,
    /// for the "Mark read" / "Mark unread" choice.
    pub async fn message_unread(&self, channel: &str, id: EventId) -> bool {
        let channel = channel.to_owned();
        self.with_actor(move |actor| {
            let Some(reads) = actor.reads.as_mut() else {
                return false;
            };
            let attention = reads.unread.attention(&channel, &id.to_hex(), &reads.sync);
            attention.unread || attention.forced
        })
        .await
        .unwrap_or(false)
    }

    /// The unread state of one channel, thread or message
    /// (`unread.snapshot(target)`), or `None` before read state loads.
    pub async fn unread_snapshot(
        &self,
        target: crate::read_state::ReadTarget,
    ) -> Option<std::sync::Arc<crate::unread::UnreadSnapshot>> {
        self.with_actor(move |actor| {
            let reads = actor.reads.as_mut()?;
            Some(reads.unread.snapshot(&target, &reads.sync))
        })
        .await
        .ok()
        .flatten()
    }

    /// Deletes an agent's message on behalf of its owner, the viewer. The
    /// caller passes the agent only after its signed profile attests the
    /// viewer as owner.
    pub async fn remove_as_owner(
        &self,
        channel: &str,
        id: EventId,
        agent: &str,
    ) -> Result<(), String> {
        self.write(super::Write::Remove {
            channel: channel.to_owned(),
            id,
            agent: Some(agent.to_owned()),
        })
        .await
    }
}
