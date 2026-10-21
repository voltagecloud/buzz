//! Channel administration, lifecycle, direct messages and search for one
//! session (`docs/port/relay.md` §4.2, §5e, §5g, §5m).
//!
//! Each operation runs as a task on the I/O runtime. It reads what it needs
//! from the actor, does its reads and writes, and hands relay-signed results
//! back to the actor, which feeds them through discovery like any other
//! event. Results started before a revocation are fenced by the access
//! epoch. Writes that confirm by polling use [`lifecycle::CONFIRM_DELAYS`]
//! within [`lifecycle::RUN_TIMEOUT`].

use super::util::{CLOSED, NOT_CONNECTED, now, post_event, sign};
use super::{Actor, Message, RosterStatus, Session};
use crate::discovery::{ChannelSummary, ResolutionStart, metadata_name, open_metadata};
use crate::dm;
use crate::event::{Verified, kinds};
use crate::http::RelayHttp;
use crate::lifecycle::{
    self, Action, ActionError, AttemptStatus, CONFIRM_DELAYS, ChannelDetails, ChannelState,
    DetailsAttempt, DetailsDraft, LifecyclePermissions, NewChannel, RUN_TIMEOUT,
};
use crate::members::{self, Person};
use crate::messages::EventTemplate;
use crate::outbox::{Delivery, Outbox, PublishError, SendOptions, accept_publish};
use crate::reader::{ReadOptions, Reader};
use crate::search::{self, Author, SearchHit, SearchRequest};
use crate::socket::Filter;
use buzz_identity::Identity;
use nostr::prelude::{Event, EventId};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

const UNCONFIRMED: &str = "This may have taken effect, but it is not confirmed yet. Refresh and check before trying again.";
const DETAILS_ACCESS: &str = "Channel access changed. Reopen channel settings.";
const LIFECYCLE_ACCESS: &str = "Channel access unavailable; refresh membership";
const TIMED_OUT: &str = "The request timed out. Try again.";
const ROSTER_UNAVAILABLE: &str = "The member list could not load. Try again.";
const ROSTER_UNCONFIRMED: &str = "The member list could not be confirmed. Try again.";

/// The viewer's hidden DM list as last read (`DmVisibility`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DmVisibilityStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Error(String),
}

/// How much owner-agent evidence a permission check reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Whatever the channel's archive state allows (`"any"`).
    Any,
    /// Only what this action needs.
    Action(Action),
    /// No profile reads.
    Skip,
}

/// Public channels read for a name search (`PUBLIC_CHANNEL_PAGE`).
const PUBLIC_CHANNEL_PAGE: usize = 500;
/// One creator's channels with one label: a small answer.
const LABELED_CHANNEL_PAGE: usize = 100;
/// Public channel matches kept per search.
const PUBLIC_CHANNEL_MATCHES: usize = 8;

/// A message search as typed: free text, a raw `from:` value (key or
/// name), a channel id and time bounds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageSearch {
    pub text: String,
    pub from: Option<String>,
    pub channel: Option<String>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    /// The viewer's selectable agents, `(pubkey, agent name)`. A `from:`
    /// name also matches an agent by its own name, as the author picker
    /// does.
    pub agents: Vec<(String, String)>,
}

/// Public channels found by name, as read-only previews.
#[derive(Clone, Debug, Default)]
pub struct PublicChannels {
    pub channels: Vec<ChannelSummary>,
    /// The relay returned a full page of channel metadata, so matches past
    /// it are missing.
    pub partial: bool,
}

/// Hits in relay relevance order, every one in a channel the viewer can
/// read now.
#[derive(Clone, Debug, Default)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// The hits' channels, joined or previewable.
    pub channels: Vec<ChannelSummary>,
    /// The `from:` name matched several people.
    pub ambiguous_author: bool,
}

/// One page of people from a profile search.
#[derive(Clone, Debug, Default)]
pub struct PeoplePage {
    pub people: Vec<Person>,
    pub more: bool,
}

/// What an operation needs from the actor to work on its own.
#[derive(Clone)]
pub(super) struct Context {
    pub(super) reader: Reader,
    pub(super) http: RelayHttp,
    pub(super) identity: Arc<Identity>,
    pub(super) viewer: String,
    pub(super) relay: String,
    pub(super) epoch: u64,
    pub(super) outbox: Outbox,
}

/// The cheap, cloneable half of a session that operations run against.
#[derive(Clone)]
pub(super) struct Ops {
    pub(super) commands: mpsc::UnboundedSender<Message>,
}

impl Session {
    pub(super) fn ops(&self) -> Ops {
        Ops {
            commands: self.commands.clone(),
        }
    }

    /// Runs `work` on the I/O runtime, whatever executor awaits it.
    pub(super) async fn run<R: Send + 'static>(
        &self,
        work: impl Future<Output = Result<R, String>> + Send + 'static,
    ) -> Result<R, String> {
        self.runtime
            .spawn(async move {
                tokio::time::timeout(RUN_TIMEOUT, work)
                    .await
                    .unwrap_or_else(|_| Err(UNCONFIRMED.to_owned()))
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }

    /// Reads profiles for these people if they are not known yet.
    pub fn want_profiles(&self, people: Vec<String>) {
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.ensure_profiles(people);
        })));
    }

    /// The agent whose message the viewer may remove as its owner: when
    /// the loaded message is not the viewer's own, the newer of the
    /// retained and a fresh signed profile of its author must attest the
    /// viewer (`useAgentOwnerEvidence`).
    pub(super) async fn removal_agent(
        &self,
        channel: &str,
        id: EventId,
    ) -> Result<Option<String>, String> {
        let ops = self.ops();
        let channel = channel.to_owned();
        let viewer = self.viewer.clone();
        self.run(async move {
            let target = ops
                .with(move |actor| {
                    let author = actor.relay.as_ref().map(|relay| relay.authority.clone());
                    actor.find_message(&channel, &id).map(|event| {
                        let signing =
                            author.and_then(|a| nostr::prelude::PublicKey::from_hex(&a).ok());
                        let shown = crate::fold::message_author(event.event(), signing.as_ref());
                        let retained = actor.profiles.event(&shown.to_hex()).cloned();
                        (event.kind_u16(), shown.to_hex(), retained)
                    })
                })
                .await?;
            let Some((kind, author, retained)) = target else {
                return Ok(None);
            };
            if kind == kinds::REACTION || author == viewer {
                return Ok(None);
            }
            let ctx = ops.context().await?;
            let events = read(&ctx, vec![members::owner_filter(&author)]).await?;
            let fresh = crate::account::newest_profile(&events, &author).cloned();
            let newest = match (fresh, retained) {
                (Some(fresh), Some(retained)) => {
                    if crate::event::is_newer(&fresh, &retained) {
                        Some(fresh)
                    } else {
                        Some(retained)
                    }
                }
                (fresh, retained) => fresh.or(retained),
            };
            Ok(newest
                .and_then(|event| crate::profiles::attested_owner(&event))
                .filter(|owner| *owner == viewer)
                .map(|_| author))
        })
        .await
    }

    /// Signs and posts a NIP-56 report of a loaded message
    /// (`messages.report`).
    pub(super) async fn report_message(
        &self,
        channel: &str,
        id: EventId,
        kind: crate::messages::ReportType,
        note: &str,
    ) -> Result<(), String> {
        let ops = self.ops();
        let (channel, note) = (channel.to_owned(), note.to_owned());
        self.run(async move {
            let original = ops
                .with(move |actor| {
                    actor
                        .find_message(&channel, &id)
                        .or_else(|| actor.thread_event(&id))
                })
                .await?
                .ok_or("Load the message before reporting it")?;
            let template = crate::messages::report(original.event(), kind, &note, now())
                .map_err(|error| error.to_string())?;
            let ctx = ops.context().await?;
            // Published directly within 10 seconds (§5d.10).
            let publish = async {
                let event = sign(&ctx.identity, template)?;
                super::util::publish(&ctx.http, &event).await
            };
            tokio::time::timeout(std::time::Duration::from_secs(10), publish)
                .await
                .unwrap_or_else(|_| Err("The report timed out.".into()))
        })
        .await
    }

    /// The channel's validated relay state: roles, permissions and details.
    /// A failed read and an answer without the channel's records read as
    /// the members dialog words them (`ChannelMembersDialog.tsx`).
    pub async fn channel_state(&self, channel: &str) -> Result<ChannelState, String> {
        let (ops, channel) = (self.ops(), channel.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let filters = lifecycle::state_filters(&ctx.relay, &channel);
            let events = read(&ctx, filters).await.map_err(|_| ROSTER_UNAVAILABLE)?;
            ChannelState::parse(&events, &channel, &ctx.relay).map_err(|error| {
                let error = error.to_string();
                if error == lifecycle::UNVERIFIED {
                    ROSTER_UNCONFIRMED.to_owned()
                } else {
                    error
                }
            })
        })
        .await
    }

    /// Reads these people's profiles now, in pages of 500, and reports
    /// whether every page answered (`profiles.ensure(…, "foreground")`).
    pub async fn load_profiles(&self, people: Vec<String>) -> Result<(), String> {
        let ops = self.ops();
        self.run(async move {
            let ctx = ops.context().await?;
            for batch in people.chunks(500) {
                let filter =
                    json!({"kinds": [kinds::PROFILE], "authors": batch, "limit": batch.len()});
                let serde_json::Value::Object(filter) = filter else {
                    unreachable!("filters are objects");
                };
                let events = read(&ctx, vec![filter]).await?;
                ops.adopt_profiles(&events).await?;
            }
            Ok(())
        })
        .await
    }

    /// What the viewer may do to the channel as a whole, including what an
    /// owner agent of theirs grants (`channel-lifecycle.ts` `load`).
    pub async fn channel_permissions(&self, channel: &str) -> Result<LifecyclePermissions, String> {
        let (ops, channel) = (self.ops(), channel.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            ops.assert_access(&channel, LIFECYCLE_ACCESS).await?;
            let permissions = ops.permissions(&ctx, &channel, Need::Any).await?;
            ops.assert_access(&channel, LIFECYCLE_ACCESS).await?;
            Ok(permissions)
        })
        .await
    }

    /// The editable details, with the version a save must still match
    /// (`channel-details.ts` `load`).
    pub async fn channel_details(&self, channel: &str) -> Result<ChannelDetails, String> {
        let (ops, channel) = (self.ops(), channel.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let (details, state) = ops.read_details(&ctx, &channel).await?;
            ops.adopt(ctx.epoch, vec![state.metadata]).await?;
            Ok(details)
        })
        .await
    }

    /// The save this session still owns for a channel, if any.
    pub fn details_attempt(&self, channel: &str) -> Option<DetailsAttempt> {
        self.details.lock().ok()?.get(channel).cloned()
    }

    fn set_details_attempt(&self, channel: &str, attempt: Option<DetailsAttempt>) {
        if let Ok(mut attempts) = self.details.lock() {
            match attempt {
                Some(attempt) => attempts.insert(channel.to_owned(), attempt),
                None => attempts.remove(channel),
            };
        }
    }

    /// Saves details edited from `version`; refused when the channel
    /// changed since. A save that may have reached the relay stays
    /// [`AttemptStatus::Unconfirmed`] until [`Self::check_channel_details`]
    /// confirms it, and no other save starts meanwhile.
    pub async fn save_channel_details(
        &self,
        channel: &str,
        version: EventId,
        draft: DetailsDraft,
    ) -> Result<(), String> {
        if self.details_attempt(channel).is_some() {
            return Err("Check the previous change before saving again.".into());
        }
        self.set_details_attempt(
            channel,
            Some(DetailsAttempt {
                draft: draft.clone(),
                status: AttemptStatus::Saving,
            }),
        );
        let published = Arc::new(AtomicBool::new(false));
        let (ops, id, flag) = (self.ops(), channel.to_owned(), published.clone());
        let saving = draft.clone();
        let labels = self
            .snapshot()
            .borrow()
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channel_labels);
        let result = self
            .run(async move { ops.save_details(&id, version, saving, labels, &flag).await })
            .await;
        match result {
            Err(_) if published.load(Ordering::SeqCst) => {
                self.set_details_attempt(
                    channel,
                    Some(DetailsAttempt {
                        draft,
                        status: AttemptStatus::Unconfirmed,
                    }),
                );
                Err(lifecycle::UNCERTAIN_SAVE.into())
            }
            result => {
                self.set_details_attempt(channel, None);
                result
            }
        }
    }

    /// Re-reads the channel for an unconfirmed save, without resending it.
    pub async fn check_channel_details(&self, channel: &str) -> Result<(), String> {
        let draft = self
            .details_attempt(channel)
            .filter(|attempt| attempt.status == AttemptStatus::Unconfirmed)
            .ok_or("No unconfirmed change to check.")?
            .draft;
        let (ops, id) = (self.ops(), channel.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let (_, state) = ops.read_details(&ctx, &id).await?;
            let confirmed = lifecycle::details_confirmed(&state, &draft);
            ops.adopt(ctx.epoch, vec![state.metadata]).await?;
            if confirmed {
                Ok(())
            } else {
                Err(lifecycle::UNCERTAIN_SAVE.to_owned())
            }
        })
        .await?;
        self.set_details_attempt(channel, None);
        Ok(())
    }

    /// Joins, leaves, archives, unarchives, deletes or hides, and waits
    /// until the relay shows it, one at a time per session. Once the
    /// command may have reached the relay, a failure is
    /// [`ActionError::Unconfirmed`].
    pub async fn channel_action(&self, action: Action, channel: &str) -> Result<(), ActionError> {
        let Ok(_busy) = self.direct.lifecycle.try_lock() else {
            return Err(ActionError::Failed(
                "Another channel action is still in progress".into(),
            ));
        };
        let published = Arc::new(AtomicBool::new(false));
        let (ops, channel, flag) = (self.ops(), channel.to_owned(), published.clone());
        let outcome = self
            .runtime
            .spawn(async move {
                tokio::time::timeout(RUN_TIMEOUT, ops.lifecycle(action, &channel, &flag)).await
            })
            .await;
        let reason = match outcome {
            Ok(Ok(Ok(()))) => return Ok(()),
            Ok(Ok(Err(reason))) => reason,
            Ok(Err(_)) => "The relay did not answer in time.".to_owned(),
            Err(_) => return Err(ActionError::Failed(CLOSED.to_owned())),
        };
        Err(if published.load(Ordering::SeqCst) {
            ActionError::Unconfirmed(reason)
        } else {
            ActionError::Failed(reason)
        })
    }

    /// Re-reads the hidden DM list (`refreshVisibility`); one read at a
    /// time. Its state is [`super::SessionSnapshot::dm_visibility`].
    pub fn refresh_dm_visibility(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.refresh_visibility();
        })));
    }

    /// Creates a stream through the outbox and returns its id once the
    /// viewer is confirmed as its owner.
    pub async fn create_channel(&self, channel: NewChannel) -> Result<String, String> {
        // A relay without NIP-CL drops label tags: refuse before any write.
        if !channel.labels.is_empty() && !self.channel_labels() {
            return Err(crate::channel_labels::UNSUPPORTED.into());
        }
        let ops = self.ops();
        self.run(async move { ops.create(channel, None).await })
            .await
    }

    /// [`Self::create_channel`] with an id the caller chose and kept, so a
    /// retry after an uncertain creation adopts that channel instead of
    /// making a second one.
    pub async fn create_channel_as(
        &self,
        channel: NewChannel,
        id: String,
    ) -> Result<String, String> {
        if !channel.labels.is_empty() && !self.channel_labels() {
            return Err(crate::channel_labels::UNSUPPORTED.into());
        }
        let ops = self.ops();
        self.run(async move { ops.create(channel, Some(id)).await })
            .await
    }

    /// One page of people to add to a channel (§5g.6).
    pub async fn search_members(&self, query: &str, page: usize) -> Result<PeoplePage, String> {
        let filter = members::search_filter(query, page)?;
        let exact = members::exact_key(query);
        let (ops, query) = (self.ops(), query.trim().to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let events = read(&ctx, vec![filter]).await?;
            ops.adopt_profiles(&events).await?;
            Ok(PeoplePage {
                more: exact.is_none() && events.len() >= members::SEARCH_PAGE,
                people: members::rank_people(&events, &query, exact.as_deref()),
            })
        })
        .await
    }

    /// One page of the New message people picker (§5e.6).
    pub async fn people(&self, query: &str, page: usize) -> Result<PeoplePage, String> {
        let (filter, limit) = dm::people_filter(query, page)?;
        let (ops, viewer) = (self.ops(), self.viewer.clone());
        self.run(async move {
            let ctx = ops.context().await?;
            let events = read(&ctx, vec![filter]).await?;
            ops.adopt_profiles(&events).await?;
            let (people, more) = dm::people_page(&events, &viewer, limit);
            Ok(PeoplePage { people, more })
        })
        .await
    }

    /// Opens (or reopens and unhides) the DM with exactly these people and
    /// returns its channel id once discovery confirms it.
    pub async fn open_dm(&self, pubkeys: Vec<String>) -> Result<String, String> {
        let ops = self.ops();
        self.run(async move { ops.open_dm(pubkeys).await }).await
    }

    /// Confirms an earlier first message of a new conversation
    /// (`directMessages.delivered`): from the outbox when it still holds
    /// it (retrying a failed or unknown send), else from a fresh relay
    /// read. The outbox owns delivery deadlines; this only follows them.
    pub async fn dm_delivered(&self, id: EventId, channel: &str) -> Result<(), String> {
        let (ops, channel) = (self.ops(), channel.to_owned());
        self.runtime
            .spawn(async move {
                let ctx = ops.context().await?;
                let Some(previous) = ctx.outbox.get(&id) else {
                    return dm_on_relay(&ctx, &id, &channel).await;
                };
                if previous.kind() != kinds::CHAT
                    || !previous
                        .event
                        .tags
                        .iter()
                        .any(|tag| tag.as_slice() == ["h".to_owned(), channel.clone()])
                {
                    return Err("The saved message does not belong to this conversation.".into());
                }
                if matches!(previous.delivery, Delivery::Failed | Delivery::Unknown) {
                    ctx.outbox
                        .retry(&id, None)
                        .map_err(|error| error.to_string())?;
                }
                let mut changes = ctx.outbox.changes();
                loop {
                    match ctx
                        .outbox
                        .get(&id)
                        .map(|op| (op.delivery, op.error.clone()))
                    {
                        Some((Delivery::Accepted | Delivery::Seen, _)) => return Ok(()),
                        Some((Delivery::Failed | Delivery::Unknown, error)) => {
                            return Err(error.unwrap_or_else(|| {
                                "The message could not be confirmed. Retry to continue.".into()
                            }));
                        }
                        // Dismissed or retired while we waited: only the
                        // relay can still say whether it landed.
                        None => return dm_on_relay(&ctx, &id, &channel).await,
                        _ => {}
                    }
                    changes.changed().await.map_err(|_| CLOSED.to_owned())?;
                }
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }

    /// Exact relay resolution of channels the viewer has not joined:
    /// readable ones come back, others are dropped.
    pub async fn resolve_channels(&self, ids: Vec<String>) -> Result<Vec<ChannelSummary>, String> {
        let ops = self.ops();
        self.run(async move {
            let ctx = ops.context().await?;
            ops.resolve(&ctx, ids).await
        })
        .await
    }

    /// The relay keeps channel labels (NIP-11 `nip-cl`).
    pub(super) fn channel_labels(&self) -> bool {
        self.snapshot()
            .borrow()
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channel_labels)
    }

    /// The channels `creator` made with `label`, oldest first
    /// (`findLabeled`, NIP-CL). Each match must be relay-signed and carry
    /// that creator and label; matches are resolved but never join the
    /// sidebar list. Refused on a relay without labels, which would ignore
    /// the filter and make "none found" a guess.
    pub async fn find_labeled(
        &self,
        creator: &str,
        label: &str,
    ) -> Result<Vec<ChannelSummary>, String> {
        if !self.channel_labels() {
            return Err(crate::channel_labels::UNSUPPORTED.into());
        }
        if creator.len() != 64
            || !creator
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err("Choose a valid channel creator.".into());
        }
        crate::channel_labels::label_tags(&[label.to_owned()])?;
        let (ops, creator, label) = (self.ops(), creator.to_owned(), label.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let page = Filter::from_iter([
                ("kinds".to_owned(), json!([kinds::CHANNEL_METADATA])),
                ("authors".to_owned(), json!([ctx.relay])),
                ("#P".to_owned(), json!([creator])),
                ("#t".to_owned(), json!([label])),
                ("limit".to_owned(), json!(LABELED_CHANNEL_PAGE)),
            ]);
            let events = read(&ctx, vec![page]).await?;
            // A label alone proves nothing; the relay-written creator does.
            let mut ids: Vec<String> = Vec::new();
            for event in &events {
                let identity =
                    crate::channel_labels::identity(event.tags.iter().map(|tag| tag.as_slice()));
                if let Some(id) = event.tag("d")
                    && event.kind_u16() == kinds::CHANNEL_METADATA
                    && event.pubkey_hex() == ctx.relay
                    && identity.creator.as_deref() == Some(creator.as_str())
                    && identity.labels.contains(&label)
                    && !ids.iter().any(|seen| seen == id)
                {
                    ids.push(id.to_owned());
                }
            }
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let mut found: Vec<ChannelSummary> = ops
                .resolve(&ctx, ids)
                .await?
                .into_iter()
                .filter(|channel| {
                    channel.creator.as_deref() == Some(creator.as_str())
                        && channel.labels.contains(&label)
                })
                .collect();
            found.sort_by(|a, b| {
                crate::channel_labels::compare_age((&a.id, a.created_at), (&b.id, b.created_at))
            });
            Ok(found)
        })
        .await
    }

    /// Open public channels the viewer has not joined whose name contains
    /// `query` (or equals it with `exact`), as read-only previews.
    pub async fn search_public_channels(
        &self,
        query: &str,
        exact: bool,
    ) -> Result<Vec<ChannelSummary>, String> {
        Ok(self
            .search_public_channel_page(query, exact)
            .await?
            .channels)
    }

    /// [`Session::search_public_channel_page`] by its other name.
    pub async fn find_public_channels(
        &self,
        query: &str,
        exact: bool,
    ) -> Result<PublicChannels, String> {
        self.search_public_channel_page(query, exact).await
    }

    /// [`Session::search_public_channels`], saying whether the relay's
    /// page of channels was full (`searchPublic`'s `partial`).
    pub async fn search_public_channel_page(
        &self,
        query: &str,
        exact: bool,
    ) -> Result<PublicChannels, String> {
        let (ops, query) = (self.ops(), query.to_owned());
        self.run(async move {
            ops.public_channels(&query, exact, PUBLIC_CHANNEL_MATCHES)
                .await
        })
        .await
    }

    /// As [`Self::search_public_channel_page`], keeping up to `limit`
    /// matches (`searchPublic`'s `limit`; at most 100).
    pub async fn search_public_channel_page_up_to(
        &self,
        query: &str,
        exact: bool,
        limit: usize,
    ) -> Result<PublicChannels, String> {
        let (ops, query) = (self.ops(), query.to_owned());
        let limit = limit.clamp(1, 100);
        self.run(async move { ops.public_channels(&query, exact, limit).await })
            .await
    }

    /// Message search (`useSearchMessages`).
    pub async fn search_messages(&self, search: MessageSearch) -> Result<SearchOutcome, String> {
        let ops = self.ops();
        self.run(async move { ops.search(search).await }).await
    }
}

/// Whether the viewer's chat message `id` reached `channel`, by an exact
/// relay read, for a send the outbox no longer holds.
async fn dm_on_relay(ctx: &Context, id: &EventId, channel: &str) -> Result<(), String> {
    let events = read(ctx, vec![dm::sent_filter(id, &ctx.viewer, channel)]).await?;
    if events.iter().any(|event| {
        event.id == *id
            && event.kind_u16() == kinds::CHAT
            && event.pubkey_hex() == ctx.viewer
            && event.has_tag("h", channel)
    }) {
        Ok(())
    } else {
        Err("The earlier send could not be confirmed. Reconnect and retry.".into())
    }
}

pub(super) async fn read(ctx: &Context, filters: Vec<Filter>) -> Result<Vec<Verified>, String> {
    ctx.reader
        .read(
            filters,
            ReadOptions {
                fresh: true,
                ..ReadOptions::default()
            },
        )
        .await
        .map(|events| events.to_vec())
        .map_err(|error| error.message)
}

/// Signs and posts a command outside the outbox. The raw result is kept
/// for receipts that carry data (41010).
async fn post(
    ctx: &Context,
    template: EventTemplate,
) -> Result<(Event, Result<Vec<u8>, crate::http::HttpError>), String> {
    let event = sign(&ctx.identity, template)?;
    let result = post_event(&ctx.http, &event).await;
    Ok((event, result))
}

/// Publishes a signed command. A refusal is final; an uncertain outcome
/// lets the caller try to confirm it anyway.
async fn publish_signed(ctx: &Context, event: &Event) -> Result<bool, String> {
    match accept_publish(post_event(&ctx.http, event).await, &event.id) {
        Ok(_) => Ok(true),
        Err(PublishError::Rejected(message)) => Err(message),
        Err(PublishError::Uncertain(_)) => Ok(false),
    }
}

/// Evaluates `$check` (an `Option`) at each confirmation delay until it
/// is `Some`. A macro, not a function: the checks borrow the context and
/// must stay `Send`.
macro_rules! confirm {
    ($check:expr) => {{
        let mut found = None;
        for delay in CONFIRM_DELAYS {
            tokio::time::sleep(delay).await;
            if let Some(value) = $check {
                found = Some(value);
                break;
            }
        }
        found
    }};
}

impl Ops {
    /// Runs `f` on the actor and returns its result.
    pub(super) async fn with<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Actor) -> R + Send + 'static,
    ) -> Result<R, String> {
        super::util::with(&self.commands, f).await
    }

    pub(super) async fn context(&self) -> Result<Context, String> {
        self.with(|actor| actor.context()).await?
    }

    /// Feeds relay-signed discovery events read at `epoch` to the actor.
    pub(super) async fn adopt(&self, epoch: u64, events: Vec<Verified>) -> Result<(), String> {
        self.with(move |actor| {
            if actor.epoch == epoch {
                actor.accept(&events);
            }
        })
        .await
    }

    async fn adopt_profiles(&self, events: &[Verified]) -> Result<(), String> {
        let profiles: Vec<Verified> = events
            .iter()
            .filter(|event| event.kind_u16() == kinds::PROFILE)
            .cloned()
            .collect();
        self.with(move |actor| {
            if actor.profiles.accept(&profiles) {
                actor.publish();
            }
        })
        .await
    }

    /// `assertAccess`: the channel is still readable to the viewer.
    pub(super) async fn assert_access(
        &self,
        channel: &str,
        message: &'static str,
    ) -> Result<(), String> {
        let id = channel.to_owned();
        let accessible = self
            .with(move |actor| {
                actor
                    .discovery
                    .as_ref()
                    .is_some_and(|discovery| discovery.can_access(&id))
            })
            .await?;
        if accessible {
            Ok(())
        } else {
            Err(message.to_owned())
        }
    }

    /// A fresh details read between access checks (`channel-details.ts`
    /// `read`).
    async fn read_details(
        &self,
        ctx: &Context,
        channel: &str,
    ) -> Result<(ChannelDetails, ChannelState), String> {
        self.assert_access(channel, DETAILS_ACCESS).await?;
        let state = self.state(ctx, channel).await?;
        self.assert_access(channel, DETAILS_ACCESS).await?;
        let details = state
            .details(&ctx.viewer)
            .map_err(|error| error.to_string())?;
        Ok((details, state))
    }

    async fn state(&self, ctx: &Context, channel: &str) -> Result<ChannelState, String> {
        let events = read(ctx, lifecycle::state_filters(&ctx.relay, channel)).await?;
        ChannelState::parse(&events, channel, &ctx.relay).map_err(|error| error.to_string())
    }

    /// Lifecycle permissions from a fresh state. When a direct role does
    /// not grant Delete (or Unarchive), the owners' profiles are read, four
    /// at a time within [`lifecycle::OWNER_EVIDENCE_TIMEOUT`]: an owner
    /// agent attested by the viewer grants it. The relay still authorizes
    /// every command.
    async fn permissions(
        &self,
        ctx: &Context,
        channel: &str,
        need: Need,
    ) -> Result<LifecyclePermissions, String> {
        let state = self.state(ctx, channel).await?;
        let mut permissions = state
            .lifecycle(&ctx.viewer)
            .map_err(|error| error.to_string())?;
        let owners = state.owner_keys();
        let wanted = match need {
            Need::Any => permissions.wants_owner_evidence(state.archived),
            Need::Action(action) => permissions.wants_evidence_for(state.archived, action),
            Need::Skip => false,
        };
        if !wanted || owners.is_empty() {
            return Ok(permissions);
        }
        let lookup = async {
            for batch in owners.chunks(lifecycle::OWNER_BATCH) {
                let events = read(ctx, lifecycle::owner_profile_filters(batch)).await?;
                if events.iter().any(|event| {
                    event.kind_u16() != kinds::PROFILE || !batch.contains(&event.pubkey_hex())
                }) {
                    return Err("Unexpected channel owner profile response".to_owned());
                }
                let owned = batch.iter().any(|owner| {
                    let newest = crate::account::newest_profile(&events, owner);
                    newest.and_then(crate::profiles::attested_owner).as_deref()
                        == Some(ctx.viewer.as_str())
                });
                if owned {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        match tokio::time::timeout(lifecycle::OWNER_EVIDENCE_TIMEOUT, lookup).await {
            Ok(Ok(true)) => permissions.grant_owned_agent(state.archived),
            Ok(Ok(false)) => {}
            // Only owner-agent actions are affected; an archived channel
            // has no Delete to retry.
            Ok(Err(_)) | Err(_) => permissions.delete_unavailable = !state.archived,
        }
        Ok(permissions)
    }

    // ----- lifecycle -----

    /// `authorize` in `channel-lifecycle.ts` `run`: `Ok(Some(events))`
    /// when a join finds the viewer already a member.
    async fn authorize_lifecycle(
        &self,
        ctx: &Context,
        action: Action,
        channel: &str,
    ) -> Result<Option<Vec<Verified>>, String> {
        if action == Action::Join {
            // Joining starts from a preview: fresh signed metadata must
            // still say public.
            let events = read(
                ctx,
                lifecycle::join_filters(&ctx.relay, &ctx.viewer, channel),
            )
            .await?;
            let member = lifecycle::join_ready(&events, &ctx.relay, &ctx.viewer, channel)
                .map_err(|error| error.to_string())?;
            return Ok(member.then_some(events));
        }
        let need = match action {
            Action::Archive | Action::Unarchive | Action::Delete => Need::Action(action),
            _ => Need::Skip,
        };
        self.assert_access(channel, LIFECYCLE_ACCESS).await?;
        let permissions = self.permissions(ctx, channel, need).await?;
        self.assert_access(channel, LIFECYCLE_ACCESS).await?;
        if permissions.delete_unavailable {
            let label = if action == Action::Delete {
                "Delete"
            } else {
                "Archive"
            };
            return Err(format!(
                "{label} check unavailable. Retry channel permissions."
            ));
        }
        if !action.allowed(&permissions) {
            return Err(match (action, permissions.leave_reason) {
                (Action::Leave, Some(reason)) => reason,
                _ => "This action is no longer permitted. Refresh channel permissions.".into(),
            });
        }
        Ok(None)
    }

    async fn lifecycle(
        &self,
        action: Action,
        channel: &str,
        published: &AtomicBool,
    ) -> Result<(), String> {
        let ctx = self.context().await?;
        if let Some(events) = self.authorize_lifecycle(&ctx, action, channel).await? {
            return self.adopt(ctx.epoch, events).await;
        }
        let template = lifecycle::template(action, channel, now()).map_err(|e| e.to_string())?;
        let event = sign(&ctx.identity, template)?;
        // Authorized again after signing, as the reference does.
        if let Some(events) = self.authorize_lifecycle(&ctx, action, channel).await? {
            return self.adopt(ctx.epoch, events).await;
        }
        // From the post until a refusal the command may have taken effect.
        published.store(true, Ordering::SeqCst);
        if let Err(error) = publish_signed(&ctx, &event).await {
            published.store(false, Ordering::SeqCst);
            return Err(error);
        }
        let mut adopted = Vec::new();
        let filter = lifecycle::confirm_filter(action, &ctx.relay, &ctx.viewer, channel);
        let events = confirm!({
            let events = read(&ctx, vec![filter.clone()]).await?;
            lifecycle::confirmed(action, &events, &ctx.relay, &ctx.viewer, channel)
                .then_some(events)
        })
        .ok_or(
            "The relay accepted the request, but the change is not confirmed. Refresh channels \
             before trying again.",
        )?;
        let channel = channel.to_owned();
        match action {
            Action::Join | Action::Archive | Action::Unarchive => {
                adopted.extend(events);
                self.adopt(ctx.epoch, adopted).await
            }
            Action::Leave | Action::Delete => self.with(move |actor| actor.removed(&channel)).await,
            Action::Hide => {
                let (relay, viewer) = (ctx.relay.clone(), ctx.viewer.clone());
                self.with(move |actor| {
                    if let Ok(Some(record)) = lifecycle::visibility_record(&events, &relay, &viewer)
                    {
                        actor.adopt_visibility(record);
                    }
                })
                .await
            }
        }
    }

    /// `authorize` in `channel-details.ts` `save`.
    async fn authorize_details(
        &self,
        ctx: &Context,
        channel: &str,
        version: EventId,
    ) -> Result<ChannelDetails, String> {
        let (details, _) = self.read_details(ctx, channel).await?;
        if !details.can_edit {
            return Err("You no longer have permission to edit this channel.".into());
        }
        if details.version != version {
            return Err("Channel details changed. Reload details before saving your edits.".into());
        }
        Ok(details)
    }

    async fn save_details(
        &self,
        channel: &str,
        version: EventId,
        draft: DetailsDraft,
        labels: bool,
        published: &AtomicBool,
    ) -> Result<(), String> {
        let ctx = self.context().await?;
        let details = self.authorize_details(&ctx, channel, version).await?;
        // A relay without NIP-CL drops label tags: refuse before signing.
        if lifecycle::labels_unsupported(&draft, &details.draft, labels) {
            return Err(crate::channel_labels::UNSUPPORTED.into());
        }
        let template = lifecycle::edit_details(channel, &draft, &details.draft, now())
            .map_err(|error| error.to_string())?;
        let event = sign(&ctx.identity, template)?;
        self.authorize_details(&ctx, channel, version).await?;
        published.store(true, Ordering::SeqCst);
        if let Err(error) = publish_signed(&ctx, &event).await {
            published.store(false, Ordering::SeqCst);
            return Err(error);
        }
        let state = confirm!({
            let (_, state) = self.read_details(&ctx, channel).await?;
            lifecycle::details_confirmed(&state, &draft).then_some(state)
        })
        .ok_or(lifecycle::UNCERTAIN_SAVE)?;
        self.adopt(ctx.epoch, vec![state.metadata]).await
    }

    async fn create(&self, channel: NewChannel, id: Option<String>) -> Result<String, String> {
        let ctx = self.context().await?;
        let filters = |id: &str| lifecycle::created_filters(&ctx.relay, &ctx.viewer, id);
        if let Some(id) = &id {
            // A chosen id may already be a channel an earlier try created.
            let events = read(&ctx, filters(id)).await?;
            if lifecycle::confirmed(Action::Join, &events, &ctx.relay, &ctx.viewer, id) {
                self.adopt(ctx.epoch, events).await?;
                return Ok(id.clone());
            }
        }
        let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let draft = lifecycle::create(&channel, &id).map_err(|error| error.to_string())?;
        let op = ctx
            .outbox
            .send(draft, SendOptions::default())
            .map_err(|error| error.to_string())?;
        // Gives up after 15 seconds; an uncertain delivery may still have
        // created the channel, which its exact receipt confirms.
        super::work_sessions::delivered(&ctx, op, false, true).await?;
        let filters = filters(&id);
        let events = confirm!({
            let events = read(&ctx, filters.clone()).await?;
            lifecycle::confirmed(Action::Join, &events, &ctx.relay, &ctx.viewer, &id)
                .then_some(events)
        })
        .ok_or(UNCONFIRMED)?;
        self.adopt(ctx.epoch, events).await?;
        Ok(id)
    }

    // ----- direct messages -----

    async fn open_dm(&self, pubkeys: Vec<String>) -> Result<String, String> {
        let ctx = self.context().await?;
        let template = dm::open_dm(&ctx.viewer, &pubkeys, now()).map_err(|e| e.to_string())?;
        let (event, result) = post(&ctx, template).await?;
        let body =
            result.map_err(|_| "The direct message could not be opened. Try again.".to_owned())?;
        let id = dm::parse_open_dm_receipt(&body, &event.id).map_err(|e| e.to_string())?;
        let events = read(&ctx, vec![dm::confirm_filter(&ctx.relay, &id)]).await?;
        self.adopt(ctx.epoch, events).await?;
        let (channel, viewer) = (id.clone(), ctx.viewer.clone());
        let confirmed = self
            .with(move |actor| {
                let summary = actor.discovery.as_ref().and_then(|d| d.get(&channel));
                let confirmed = dm::confirmed(summary.as_ref(), &viewer, &pubkeys);
                if confirmed {
                    // The relay unhides a reopened DM.
                    actor.unhide(&channel);
                    actor.sidebar_opened(&channel);
                }
                confirmed
            })
            .await?;
        if !confirmed {
            return Err(
                "The conversation’s participants could not be confirmed. Try again.".into(),
            );
        }
        Ok(id)
    }

    // ----- channels and search -----

    pub(super) async fn resolve(
        &self,
        ctx: &Context,
        ids: Vec<String>,
    ) -> Result<Vec<ChannelSummary>, String> {
        let wanted = ids.clone();
        let plan = self
            .with(move |actor| actor.plan_resolution(&ids))
            .await??;
        if let Some((targets, filters, start)) = plan {
            let events = read(ctx, filters).await?;
            let epoch = ctx.epoch;
            self.with(move |actor| actor.finish_resolution(epoch, &targets, &start, &events))
                .await??;
        }
        self.with(move |actor| {
            let Some(discovery) = &actor.discovery else {
                return Vec::new();
            };
            wanted.iter().filter_map(|id| discovery.get(id)).collect()
        })
        .await
    }

    async fn public_channels(
        &self,
        query: &str,
        exact: bool,
        limit: usize,
    ) -> Result<PublicChannels, String> {
        // One leading `#`, after lowercasing (`replace(/^#/, "")`).
        let lower = query.trim().to_lowercase();
        let needle = lower.strip_prefix('#').unwrap_or(&lower).to_owned();
        if needle.is_empty() {
            return Ok(PublicChannels::default());
        }
        let ctx = self.context().await?;
        // Only once the channel list is in (`list.status`), so a channel
        // you joined never shows as one to join.
        if !self
            .with(|actor| actor.roster == RosterStatus::Ready)
            .await?
        {
            return Err("Channel list is not ready for channel search".to_owned());
        }
        let page = Filter::from_iter([
            ("kinds".to_owned(), json!([kinds::CHANNEL_METADATA])),
            ("authors".to_owned(), json!([ctx.relay])),
            ("limit".to_owned(), json!(PUBLIC_CHANNEL_PAGE)),
        ]);
        let events = read(&ctx, vec![page]).await?;
        let partial = events.len() >= PUBLIC_CHANNEL_PAGE;
        let joined: HashSet<String> = self
            .with(|actor| {
                actor
                    .discovery
                    .as_ref()
                    .map(|d| d.channels().into_iter().map(|c| c.id).collect())
                    .unwrap_or_default()
            })
            .await?;
        let mut newest: std::collections::HashMap<&str, &Verified> = Default::default();
        for event in events
            .iter()
            .filter(|e| e.kind_u16() == kinds::CHANNEL_METADATA && e.pubkey_hex() == ctx.relay)
        {
            if let Some(id) = event.tag("d")
                && newest
                    .get(id)
                    .is_none_or(|current| crate::event::is_newer(event, current))
            {
                newest.insert(id, event);
            }
        }
        let mut candidates: Vec<(u8, String, String)> = newest
            .into_iter()
            .filter(|(id, event)| {
                open_metadata(event)
                    && event.tag("archived") != Some("true")
                    && !joined.contains(*id)
            })
            .filter_map(|(id, event)| {
                let name = metadata_name(event)?;
                let lower = name.to_lowercase();
                let matches = if exact {
                    lower == needle
                } else {
                    lower.contains(&needle)
                };
                matches.then(|| {
                    let rank = search::match_rank(&name, &needle).unwrap_or(3);
                    (rank, name, id.to_owned())
                })
            })
            .collect();
        // Rank, then name and id as `localeCompare` orders them.
        candidates.sort_by(|a, b| {
            use crate::discovery::locale_compare;
            a.0.cmp(&b.0)
                .then_with(|| locale_compare(&a.1, &b.1))
                .then_with(|| locale_compare(&a.2, &b.2))
        });
        candidates.truncate(limit);
        let ids: Vec<String> = candidates.into_iter().map(|(_, _, id)| id).collect();
        let mut channels: Vec<ChannelSummary> = self
            .resolve(&ctx, ids.clone())
            .await?
            .into_iter()
            .filter(|c| c.read_only && !c.archived && !c.cached)
            .collect();
        // Best match first, as ranked above.
        channels.sort_by_key(|c| ids.iter().position(|id| *id == c.id));
        Ok(PublicChannels { channels, partial })
    }

    async fn search(&self, search: MessageSearch) -> Result<SearchOutcome, String> {
        let ctx = self.context().await?;
        let mut outcome = SearchOutcome::default();
        let author = match search.from.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(key) if search::is_hex_key(key) => Some(key.to_ascii_lowercase()),
            Some(name) => {
                let handle = name.strip_prefix('@').unwrap_or(name).to_owned();
                match self
                    .author(&ctx, &handle, search.channel.clone(), search.agents.clone())
                    .await?
                {
                    Author::Found(key) => Some(key),
                    Author::Missing => return Ok(outcome),
                    Author::Ambiguous => {
                        outcome.ambiguous_author = true;
                        return Ok(outcome);
                    }
                }
            }
        };
        if let Some(channel) = &search.channel
            && self.resolve(&ctx, vec![channel.clone()]).await?.is_empty()
        {
            return Ok(outcome);
        }
        let request = SearchRequest {
            text: search.text.trim().to_owned(),
            author,
            channel: search.channel.clone(),
            since: search.since,
            until: search.until,
        };
        if request.is_empty() {
            return Ok(outcome);
        }
        let events = read(&ctx, vec![request.filter()]).await?;
        let hits = search::hits(&events, search.channel.as_deref(), Some(&ctx.relay));
        let mut ids: Vec<String> = hits.iter().map(|hit| hit.channel.clone()).collect();
        ids.sort();
        ids.dedup();
        ids.truncate(crate::discovery::RESOLVE_LIMIT);
        outcome.channels = self.resolve(&ctx, ids).await?;
        let readable: HashSet<&str> = outcome.channels.iter().map(|c| c.id.as_str()).collect();
        outcome.hits = hits
            .into_iter()
            .filter(|hit| readable.contains(hit.channel.as_str()))
            .collect();
        let authors = outcome
            .hits
            .iter()
            .flat_map(|hit| std::iter::once(hit.author.clone()).chain(hit.workflow_owner.clone()))
            .collect();
        self.with(move |actor| actor.ensure_profiles(authors))
            .await?;
        Ok(outcome)
    }

    /// Resolves a `from:` name: the scoped channel's members first, then a
    /// relay profile lookup. Selectable agents are candidates either way,
    /// by profile name or by their own name.
    async fn author(
        &self,
        ctx: &Context,
        handle: &str,
        channel: Option<String>,
        agents: Vec<(String, String)>,
    ) -> Result<Author, String> {
        let wanted = handle.to_owned();
        let (scoped, agents) = self
            .with(move |actor| {
                let members = channel
                    .and_then(|id| actor.discovery.as_ref()?.get(&id))
                    .map(|c| c.members)
                    .unwrap_or_default();
                let profiles = actor.profiles.snapshot();
                let named: Vec<(String, String)> = members
                    .into_iter()
                    .filter_map(|key| {
                        let name = profiles.get(&key)?.name.clone();
                        Some((key, name))
                    })
                    .collect();
                let mut aliases = Vec::new();
                for (key, name) in agents {
                    if let Some(profile) = profiles.get(&key) {
                        aliases.push((key.clone(), profile.name.clone()));
                    }
                    aliases.push((key, name));
                }
                let scoped = match search::resolve_author(
                    &wanted,
                    named.iter().map(|(k, n)| (k.as_str(), n.as_str())),
                ) {
                    Author::Missing => None,
                    _ => Some(named),
                };
                (scoped, aliases)
            })
            .await?;
        let pairs = |people: &[(String, String)]| {
            search::resolve_author(
                handle,
                people
                    .iter()
                    .chain(&agents)
                    .map(|(k, n)| (k.as_str(), n.as_str())),
            )
        };
        if let Some(scoped) = scoped {
            return Ok(pairs(&scoped));
        }
        let events = read(ctx, vec![search::author_lookup_filter(handle)]).await?;
        self.adopt_profiles(&events).await?;
        let people: Vec<(String, String)> = members::rank_people(&events, handle, None)
            .into_iter()
            .map(|p| (p.pubkey, p.profile.name))
            .collect();
        Ok(pairs(&people))
    }
}

impl Actor {
    /// The relay has not answered yet: [`Self::context`] fails with
    /// `NOT_CONNECTED`, not because only a saved snapshot shows.
    pub(super) fn awaiting_relay(&self) -> bool {
        match &self.relay {
            None => !self.cached,
            Some(_) => self.outbox.is_none(),
        }
    }

    pub(super) fn context(&self) -> Result<Context, String> {
        let (Some(relay), Some(outbox)) = (&self.relay, &self.outbox) else {
            // A saved snapshot reads nothing (`cachedOnly`).
            return Err(if self.awaiting_relay() {
                NOT_CONNECTED
            } else {
                super::channels::RECONNECT_TO_UPDATE
            }
            .into());
        };
        Ok(Context {
            reader: self.reader.clone(),
            http: self.http.clone(),
            identity: self.identity.clone(),
            viewer: self.identity.pubkey_hex(),
            relay: relay.authority.clone(),
            epoch: self.epoch,
            outbox: outbox.clone(),
        })
    }

    /// The viewer left: the channel is gone from discovery and its views.
    fn removed(&mut self, channel: &str) {
        if let Some(discovery) = &mut self.discovery {
            discovery.deny(channel);
        }
        self.purge_denied();
        self.update_interests();
        self.reads_channels(false);
        self.publish();
    }

    // ----- exact resolution (`store.ts` `resolve`) -----

    #[allow(clippy::type_complexity)]
    fn plan_resolution(
        &self,
        ids: &[String],
    ) -> Result<Option<(Vec<String>, Vec<Filter>, ResolutionStart)>, String> {
        let discovery = self.discovery.as_ref().ok_or(NOT_CONNECTED)?;
        let targets = discovery
            .resolve_targets(ids)
            .map_err(|error| error.to_string())?;
        if targets.is_empty() {
            return Ok(None);
        }
        let filters = crate::discovery::resolve_filters(
            discovery.relay_author(),
            discovery.viewer(),
            &targets,
            true,
        );
        let start = discovery.resolution_start(&targets);
        Ok(Some((targets, filters, start)))
    }

    fn finish_resolution(
        &mut self,
        epoch: u64,
        targets: &[String],
        start: &ResolutionStart,
        events: &[Verified],
    ) -> Result<(), String> {
        if epoch != self.epoch {
            return Ok(());
        }
        let Some(discovery) = &mut self.discovery else {
            return Ok(());
        };
        let resolution = discovery
            .apply_resolution(targets, start, events)
            .map_err(|error| error.to_string())?;
        if resolution.applied.changed || !resolution.denied.is_empty() || resolution.resumed {
            self.purge_denied();
            self.update_interests();
            self.reads_channels(false);
            // A resumed preview's recheck notice goes with it.
            self.publish_live();
            self.publish();
        }
        Ok(())
    }

    // ----- hidden DMs (30622) -----

    /// Re-reads the viewer's hidden DM list (`refreshVisibility`): one
    /// read at a time; a failure or invalid list keeps what is shown and
    /// reports the error.
    pub(super) fn refresh_visibility(&mut self) {
        if self.visibility_reading {
            return;
        }
        let Ok(ctx) = self.context() else {
            return;
        };
        self.visibility_reading = true;
        self.set_visibility_status(DmVisibilityStatus::Loading);
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let filter = lifecycle::visibility_filter(&ctx.relay, &ctx.viewer, false);
            let read = tokio::time::timeout(RUN_TIMEOUT, read(&ctx, vec![filter]))
                .await
                .unwrap_or_else(|_| Err(TIMED_OUT.to_owned()));
            let result = read.and_then(|events| {
                lifecycle::visibility_record(&events, &ctx.relay, &ctx.viewer)
                    .map_err(|error| error.to_string())
            });
            let _ = commands.send(Message::With(Box::new(move |actor| {
                actor.visibility_reading = false;
                if actor.epoch != ctx.epoch {
                    actor.set_visibility_status(DmVisibilityStatus::Idle);
                    return;
                }
                match result {
                    Ok(Some(record)) => actor.adopt_visibility(record),
                    Ok(None) => {
                        if actor.visibility_record.is_none() {
                            actor.set_hidden_dms(Vec::new());
                        }
                        actor.set_visibility_status(DmVisibilityStatus::Ready);
                    }
                    Err(error) => actor.set_visibility_status(DmVisibilityStatus::Error(error)),
                }
            })));
        });
    }

    /// A newer valid list replaces the shown one; an older one is ignored.
    fn adopt_visibility(&mut self, record: lifecycle::VisibilityRecord) {
        if self
            .visibility_record
            .as_ref()
            .is_none_or(|current| record.newer_than(current))
        {
            self.set_hidden_dms(record.hidden.clone());
            self.visibility_record = Some(record);
        }
        self.set_visibility_status(DmVisibilityStatus::Ready);
    }

    fn set_visibility_status(&mut self, status: DmVisibilityStatus) {
        if self.dm_visibility != status {
            self.dm_visibility = status;
            self.publish();
        }
    }

    fn set_hidden_dms(&mut self, mut hidden: Vec<String>) {
        hidden.sort();
        hidden.dedup();
        if *self.hidden_dms != *hidden {
            self.hidden_dms = hidden.into();
            self.publish();
        }
    }

    fn unhide(&mut self, channel: &str) {
        if self.hidden_dms.iter().any(|id| id == channel) {
            let hidden = self
                .hidden_dms
                .iter()
                .filter(|id| *id != channel)
                .cloned()
                .collect();
            self.set_hidden_dms(hidden);
        }
    }
}
