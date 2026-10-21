//! Newly arrived messages that deserve a notification (spec §5l.1).
//!
//! A live message from someone else becomes a candidate when it was not
//! seen before; after it is accepted, unread decides its attention. Mentions,
//! direct messages and replies in the viewer's conversations are held until
//! read state is known and any conversation lookup has decided, then sent on
//! [`Session::incoming`] if they are still unread, or in view (the app
//! decides whether those notify). Anything older than two minutes is
//! dropped. Muted channels hold back everything but explicit mentions, and
//! nothing but mentions is sent while the mutes are still unknown
//! (`messages.ts`). The user's notification preferences are the app's to
//! apply.
//!
//! [`Session::incoming`]: super::Session::incoming

use super::Actor;
use super::unread::now_ms;
use crate::audience::{Audience, audience};
use crate::event::{Verified, kinds};
use crate::read_state::{Completeness, ReadCapability, ReadSyncStatus};
use crate::unread::{AttentionStatus, Category, preview_text, workflow_owner};
use nostr::prelude::EventId;
use std::collections::{HashSet, VecDeque};

/// Candidates older than this (or further in the future than
/// [`MAX_SKEW_MS`]) never notify.
const FRESH_MS: u64 = 120_000;
const MAX_SKEW_MS: u64 = 30_000;
/// Most candidates held while read state or a lookup is pending.
const MAX_PENDING: usize = 128;
/// Message ids remembered so a redelivery is not a new arrival.
const RECENT: usize = 2048;
/// Longest preview handed to the app.
const PREVIEW_CHARS: usize = 4096;

/// A message that should notify, once the app's preferences agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingMessage {
    pub channel_id: String,
    pub message_id: EventId,
    pub created_at: u64,
    pub author: String,
    /// Trusted workflow output names its owner; show them as the sender.
    pub workflow_owner: Option<String>,
    /// The message text (kind 40002: its body), at most 4096 characters.
    pub preview: String,
    /// The signed audience: agents is coordination between agents.
    pub audience: Option<Audience>,
    pub category: Category,
    /// Mentions the viewer: channel mute does not apply.
    pub mentioned: bool,
    pub thread_root: Option<String>,
    /// The message is in view in the open channel ("Notify while viewing"
    /// decides whether it still notifies).
    pub viewing: bool,
}

pub(super) struct Candidate {
    event: Verified,
    channel: String,
    category: Category,
}

#[derive(Default)]
pub(super) struct Incoming {
    pending: Vec<Candidate>,
    recent: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl Incoming {
    /// Remembers `id`; false if it was seen already.
    fn first_sight(&mut self, id: EventId) -> bool {
        if !self.recent.insert(id) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > RECENT {
            if let Some(old) = self.order.pop_front() {
                self.recent.remove(&old);
            }
        }
        true
    }
}

fn fresh(event: &Verified, now: u64) -> bool {
    let at = event.created_at_secs().saturating_mul(1000);
    at <= now + MAX_SKEW_MS && now.saturating_sub(at) <= FRESH_MS
}

impl Actor {
    /// Live messages from others that are new to this session. Call before
    /// the batch is accepted: evidence already retained is not new.
    pub(super) fn incoming_candidates(
        &mut self,
        events: &[Verified],
        channel: Option<&str>,
    ) -> Vec<Verified> {
        // Only traffic attributed to one channel can notify.
        let (Some(reads), Some(scope)) = (&self.reads, channel) else {
            return Vec::new();
        };
        let viewer = self.identity.pubkey_hex();
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind_u16(),
                    kinds::CHAT | kinds::AGENT_MESSAGE | kinds::DIFF
                ) && event.pubkey_hex() != viewer
                    && event.tags_named("h").count() == 1
                    && event.channel() == Some(scope)
                    && !reads.unread.retains(&event.id_hex())
            })
            .filter(|event| self.incoming.first_sight(event.id))
            .cloned()
            .collect()
    }

    /// Admits accepted candidates that unread classifies as attention.
    pub(super) fn admit_incoming(&mut self, candidates: Vec<Verified>) {
        if candidates.is_empty() {
            return;
        }
        let now = now_ms();
        let (Some(reads), Some(discovery)) = (&mut self.reads, &self.discovery) else {
            return;
        };
        for event in candidates {
            let Some(channel) = event.channel().map(str::to_owned) else {
                continue;
            };
            if !discovery.can_access(&channel) || !fresh(&event, now) {
                continue;
            }
            let attention = reads
                .unread
                .attention(&channel, &event.id_hex(), &reads.sync);
            // A reply whose conversation lookup is pending may still be
            // thread attention.
            let category = attention
                .category
                .or(attention.pending.then_some(Category::Thread));
            let Some(category) = category else {
                continue;
            };
            if (attention.status != AttentionStatus::Eligible && !attention.pending)
                || self.incoming.pending.len() >= MAX_PENDING
            {
                continue;
            }
            self.incoming.pending.push(Candidate {
                event,
                channel,
                category,
            });
        }
        self.pump_incoming();
    }

    /// Re-checks held candidates (`eligible()` in `messages.ts`): sends the
    /// ones still unread and out of view, drops the ones that no longer
    /// qualify, and keeps waiting on unknown read state.
    pub(super) fn pump_incoming(&mut self) {
        if self.incoming.pending.is_empty() {
            return;
        }
        let now = now_ms();
        let authority = self.relay.as_ref().map(|relay| relay.authority.clone());
        let (Some(reads), Some(discovery)) = (&mut self.reads, &self.discovery) else {
            return;
        };
        let sync = reads.sync.snapshot().clone();
        let sync_unknown = matches!(sync.status, ReadSyncStatus::Loading | ReadSyncStatus::Error)
            || (sync.capability != ReadCapability::Unsupported
                && sync.completeness == Completeness::Unknown);
        for candidate in std::mem::take(&mut self.incoming.pending) {
            if !fresh(&candidate.event, now) || !discovery.can_access(&candidate.channel) {
                continue;
            }
            let id = candidate.event.id_hex();
            let attention = reads.unread.attention(&candidate.channel, &id, &reads.sync);
            // Explicit mentions bypass channel mute; unknown mutes must
            // not briefly release ordinary alerts.
            if !attention.mentioned {
                match self.sidebar.muted(&candidate.channel) {
                    Some(true) => continue,
                    Some(false) => {}
                    None => {
                        self.incoming.pending.push(candidate);
                        continue;
                    }
                }
            }
            if attention.status == AttentionStatus::Ineligible
                || (!attention.pending && attention.category != Some(candidate.category))
            {
                continue;
            }
            if sync_unknown || attention.status == AttentionStatus::Unknown {
                self.incoming.pending.push(candidate);
                continue;
            }
            // A row read by the open view's reading dwell still notifies
            // while in view, if the user asked for that.
            if !attention.unread && !attention.viewing {
                continue;
            }
            let event = &candidate.event;
            let _ = self.incoming_sender.send(IncomingMessage {
                channel_id: candidate.channel.clone(),
                message_id: event.id,
                created_at: event.created_at_secs(),
                author: event.pubkey_hex(),
                workflow_owner: workflow_owner(event, authority.as_deref()),
                preview: preview_text(event).chars().take(PREVIEW_CHARS).collect(),
                audience: audience(event),
                category: candidate.category,
                mentioned: attention.mentioned,
                thread_root: attention.root_id.clone(),
                viewing: attention.viewing,
            });
        }
    }
}
