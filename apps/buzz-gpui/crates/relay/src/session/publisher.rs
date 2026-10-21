//! The session's outbox publisher: HTTP delivery with the mention preflight
//! of `session.ts` (`prepareMentionPublication`). A message that mentions
//! people is only dispatched after a fresh, strong read of the channel
//! roster proves every recipient is still a member, and only if nothing
//! about access changed while it was read.

use super::{Actor, Message};
use crate::event::{Verified, first_tag, is_hex64, kinds};
use crate::http::HttpError;
use crate::outbox::{HttpPublisher, PublishError, Publisher};
use crate::reader::{Priority, ReadError, ReadOptions, Reader};
use futures::future::BoxFuture;
use nostr::prelude::{Event, EventId};
use serde_json::json;
use tokio::sync::mpsc;

const CHANGED: &str = "Channel membership changed during mention verification; retry";
const UNVERIFIED: &str = "Could not verify current channel membership; refresh and retry";

pub(super) struct SessionPublisher {
    pub http: HttpPublisher,
    pub reader: Reader,
    pub commands: mpsc::UnboundedSender<Message>,
    pub relay: String,
    pub viewer: String,
    pub workflows: crate::workflows::Workflows,
}

/// What a preflight compares before and after its read.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fence {
    epoch: u64,
    live: u64,
}

impl SessionPublisher {
    async fn on_actor<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Actor) -> R + Send + 'static,
    ) -> Result<R, PublishError> {
        super::util::with(&self.commands, f)
            .await
            .map_err(PublishError::Rejected)
    }

    /// A fresh, strong read of `channel`'s relay-signed roster.
    async fn read_roster(&self, channel: &str) -> Result<crate::Events, ReadError> {
        let filter = json!({
            "kinds": [kinds::CHANNEL_MEMBERS],
            "consistency": "strong",
            "authors": [self.relay],
            "#d": [channel],
            "limit": 1,
        });
        let serde_json::Value::Object(filter) = filter else {
            unreachable!("filters are objects")
        };
        let options = ReadOptions {
            priority: Priority::Foreground,
            fresh: true,
        };
        self.reader.read(vec![filter], options).await
    }

    /// Checks the mention against the session's roster and returns the
    /// fence to compare after the read.
    async fn check(&self, event: Event) -> Result<Fence, PublishError> {
        self.on_actor(move |actor| {
            actor
                .validate_mention_event(&event)
                .map(|()| Fence {
                    epoch: actor.epoch,
                    live: actor.channels.live_generation,
                })
                .map_err(PublishError::Rejected)
        })
        .await?
    }
}

const CANVAS_ACCESS: &str = "Canvas access changed; your draft is kept";
const CANVAS_MEMBERSHIP: &str = "Could not refresh channel membership. Retry to continue.";

impl SessionPublisher {
    async fn can_participate(&self, channel: &str) -> Result<bool, PublishError> {
        let channel = channel.to_owned();
        self.on_actor(move |actor| {
            actor
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_participate(&channel))
        })
        .await
    }

    /// A Canvas save is only dispatched after a strong read of the
    /// channel roster still shows the viewer may write there
    /// (`session.ts` `preparePublish`, `refreshMembership`).
    async fn prepare_canvas(&self, event: &Event) -> Result<(), PublishError> {
        let channel = first_tag(event, "h")
            .map(str::to_owned)
            .ok_or_else(|| PublishError::Rejected("Canvas has no channel".into()))?;
        if !self.can_participate(&channel).await? {
            return Err(PublishError::Rejected(CANVAS_ACCESS.into()));
        }
        let events = self
            .read_roster(&channel)
            .await
            .map_err(|_| PublishError::Rejected(CANVAS_MEMBERSHIP.into()))?;
        let roster = events
            .iter()
            .find(|event| {
                event.kind_u16() == kinds::CHANNEL_MEMBERS
                    && event.pubkey_hex() == self.relay
                    && event.has_tag("d", &channel)
            })
            .cloned()
            .ok_or_else(|| PublishError::Rejected(CANVAS_MEMBERSHIP.into()))?;
        let allowed = self
            .on_actor(move |actor| {
                actor.accept(std::slice::from_ref(&roster));
                actor
                    .discovery
                    .as_ref()
                    .is_some_and(|discovery| discovery.can_participate(&channel))
            })
            .await?;
        if !allowed {
            return Err(PublishError::Rejected(CANVAS_ACCESS.into()));
        }
        Ok(())
    }
}

impl Publisher for SessionPublisher {
    fn post_event(&self, event: &Event) -> BoxFuture<'static, Result<Vec<u8>, HttpError>> {
        self.http.post_event(event)
    }

    fn find_event(&self, event: &Event) -> BoxFuture<'static, Result<bool, String>> {
        self.http.find_event(event)
    }

    fn read_echo(&self, id: EventId, priority: Priority) -> BoxFuture<'static, Vec<Verified>> {
        self.http.read_echo(id, priority)
    }

    fn prepare(&self, event: &Event) -> BoxFuture<'static, Result<(), PublishError>> {
        // Workflow commands are checked again at publish start: access may
        // have changed since they were queued.
        let tags: Vec<&[String]> = event.tags.iter().map(|tag| tag.as_slice()).collect();
        if let Err(error) = self.workflows.validate(
            event.kind.as_u16(),
            &event.pubkey.to_hex(),
            &event.content,
            &tags,
        ) {
            return Box::pin(async move { Err(PublishError::Rejected(error)) });
        }
        let this = SessionPublisher {
            http: self.http.clone(),
            reader: self.reader.clone(),
            commands: self.commands.clone(),
            relay: self.relay.clone(),
            viewer: self.viewer.clone(),
            workflows: self.workflows.clone(),
        };
        if event.kind.as_u16() == kinds::CANVAS {
            let event = event.clone();
            return Box::pin(async move { this.prepare_canvas(&event).await });
        }
        let mentions =
            event.kind.as_u16() == kinds::CHAT && event.tags.iter().any(|tag| tag.kind() == "p");
        if !mentions {
            return Box::pin(async { Ok(()) });
        }
        let event = event.clone();
        Box::pin(async move {
            let fence = this.check(event.clone()).await?;
            let channel = first_tag(&event, "h")
                .map(str::to_owned)
                .ok_or_else(|| PublishError::Rejected("A channel is required".into()))?;
            // An empty or failed read is unknown, never permission.
            let events = this
                .read_roster(&channel)
                .await
                .map_err(|_| PublishError::Rejected(UNVERIFIED.into()))?;
            let roster = match &events[..] {
                [roster]
                    if roster.kind_u16() == kinds::CHANNEL_MEMBERS
                        && roster.pubkey_hex() == this.relay
                        && roster.tags_named("d").count() == 1
                        && roster
                            .tags_named("d")
                            .all(|tag| tag.len() == 2 && tag[1] == channel) =>
                {
                    roster.clone()
                }
                _ => return Err(PublishError::Rejected(UNVERIFIED.into())),
            };
            let (viewer, event_check) = (this.viewer.clone(), event.clone());
            this.on_actor(move |actor| {
                let now = Fence {
                    epoch: actor.epoch,
                    live: actor.channels.live_generation,
                };
                if now != fence {
                    return Err(PublishError::Rejected(CHANGED.into()));
                }
                actor.accept(std::slice::from_ref(&roster));
                // A newer live removal beats an older read result.
                actor
                    .validate_mention_event(&event_check)
                    .map_err(PublishError::Rejected)?;
                let members = |key: &str| roster.has_tag("p", key);
                if !members(&viewer)
                    || event_check
                        .tags
                        .iter()
                        .filter(|tag| tag.kind() == "p")
                        .any(|tag| tag.as_slice().get(1).is_none_or(|key| !members(key)))
                {
                    return Err(PublishError::Rejected(
                        "A selected recipient is no longer a channel member; remove them or refresh membership"
                            .into(),
                    ));
                }
                Ok(())
            })
            .await?
        })
    }
}

impl Actor {
    /// `validateMentionEvent`: a mentioning message names at most 32 valid
    /// recipients, one channel, and only current members of a channel the
    /// viewer may post in, while live updates are connected.
    pub(super) fn validate_mention_event(&self, event: &Event) -> Result<(), String> {
        if event.kind.as_u16() != kinds::CHAT {
            return Ok(());
        }
        let recipients: Vec<&[String]> = event
            .tags
            .iter()
            .filter(|tag| tag.kind() == "p")
            .map(|tag| tag.as_slice())
            .collect();
        if recipients.is_empty() {
            return Ok(());
        }
        let valid = |value: &String| is_hex64(value);
        if recipients.len() > 32
            || recipients
                .iter()
                .any(|tag| tag.len() != 2 || !valid(&tag[1]))
        {
            return Err("Invalid mention recipients".into());
        }
        let destinations: Vec<&String> = event
            .tags
            .iter()
            .filter(|tag| tag.kind() == "h")
            .filter_map(|tag| tag.as_slice().get(1))
            .collect();
        let [channel] = destinations[..] else {
            return Err("A channel is required".into());
        };
        if channel.is_empty() {
            return Err("A channel is required".into());
        }
        let discovery = self
            .discovery
            .as_ref()
            .ok_or("Refresh channel membership before mentioning anyone")?;
        if self.live.as_ref().is_some_and(|live| !live.is_connected()) {
            return Err("Refresh channel membership before mentioning anyone".into());
        }
        let keys: Vec<String> = recipients.iter().map(|tag| tag[1].clone()).collect();
        crate::members::validate_mentions(
            discovery.get(channel).as_ref(),
            discovery.can_participate(channel),
            &keys,
        )
    }
}
