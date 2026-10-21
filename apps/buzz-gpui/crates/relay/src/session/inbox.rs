//! The inbox (`inbox.ts`, `inbox-feed.ts`): the unread owner's inbox
//! projection, and a bounded history of messages that mention the viewer.
//!
//! The history read goes through the session's ordinary admission, so its
//! messages become unread evidence like any other; the feed itself keeps
//! only its status and the exact messages whose edits and deletions are
//! still being checked ("Preview updating…").

use super::{Actor, Message, Session};
use crate::event::{Verified, kinds};
use crate::read_state::{ReadCapability, ReadTarget};
use crate::reader::{ReadError, ReadOptions};
use crate::socket::{Filter, filter};
use crate::unread::{InboxItem, InboxSnapshot, InboxStatus};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{oneshot, watch};

use super::util::CLOSED;
/// Addressed chat kinds the history reads.
const MENTION_KINDS: [u16; 2] = [kinds::CHAT, kinds::AGENT_MESSAGE];
const OVERLAY_PAGE: u64 = 500;
const OVERLAY_EVENTS: usize = 2000;
const OVERLAY_BYTES: usize = 4 * 1024 * 1024;
/// An action that outlived its row.
pub const EXPIRED: &str = "Inbox action expired. Close and reopen the conversation.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxFeedStatus {
    Idle,
    Loading,
    Ready,
    Error,
}

/// The history read's state (`InboxFeedSnapshot`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxFeed {
    pub status: InboxFeedStatus,
    pub error: Option<String>,
    /// Addressed messages whose stored edits and deletions are not settled.
    pub incomplete: Vec<String>,
}

impl Default for InboxFeed {
    fn default() -> Self {
        Self {
            status: InboxFeedStatus::Idle,
            error: None,
            incomplete: Vec::new(),
        }
    }
}

/// What the Inbox page reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxState {
    pub inbox: Arc<InboxSnapshot>,
    pub feed: InboxFeed,
    /// Read-state sync can save reads (`frontier-sync`).
    pub can_read: bool,
}

impl Default for InboxState {
    fn default() -> Self {
        Self {
            inbox: Arc::new(InboxSnapshot {
                status: InboxStatus::Idle,
                items: Vec::new(),
                freshness: crate::unread::Freshness::Unknown,
                error: None,
            }),
            feed: InboxFeed::default(),
            can_read: false,
        }
    }
}

/// The actor's side.
pub(super) struct Owner {
    sender: watch::Sender<Arc<InboxState>>,
    /// Someone watches the inbox.
    wanted: bool,
    feed: InboxFeed,
    /// Bumped by stale, clear and dispose; fences old reads.
    epoch: u64,
    running: bool,
    /// The history was asked for, so a reconnect reads it again.
    requested: bool,
}

pub(super) fn new() -> (Owner, watch::Receiver<Arc<InboxState>>) {
    let (sender, receiver) = watch::channel(Arc::new(InboxState::default()));
    (
        Owner {
            sender,
            wanted: false,
            feed: InboxFeed::default(),
            epoch: 0,
            running: false,
            requested: false,
        },
        receiver,
    )
}

/// The addressed history read: 50 newest chat messages naming the viewer.
pub fn mentions_filter(viewer: &str) -> Filter {
    filter(json!({"kinds": MENTION_KINDS, "#p": [viewer], "limit": 50}))
}

fn overlay_filter(ids: &[String], kinds: &[u16], cursor: Option<(u64, String)>) -> Filter {
    let mut filter = json!({"kinds": kinds, "#e": ids, "limit": OVERLAY_PAGE});
    if let Some((until, before)) = cursor {
        filter["until"] = json!(until);
        filter["before_id"] = json!(before);
    }
    crate::socket::filter(filter)
}

/// `older`: strictly past the cursor in (created_at desc, id asc) order.
fn older(event: &Verified, cursor: &(u64, String)) -> bool {
    let at = event.created_at_secs();
    at < cursor.0 || (at == cursor.0 && event.id_hex() > cursor.1)
}

/// Reads every page of `kinds` events that reference `ids`.
async fn overlays(
    reader: &crate::reader::Reader,
    ids: &[String],
    kinds: &[u16],
) -> Result<Vec<Verified>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut collected: Vec<Verified> = Vec::new();
    let mut cursor: Option<(u64, String)> = None;
    loop {
        let mut page: Vec<Verified> = reader
            .read(
                vec![overlay_filter(ids, kinds, cursor.clone())],
                ReadOptions {
                    fresh: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error: ReadError| error.message)?
            .to_vec();
        page.sort_by(|a, b| {
            b.created_at_secs()
                .cmp(&a.created_at_secs())
                .then_with(|| a.id_hex().cmp(&b.id_hex()))
        });
        if let Some(previous) = &cursor
            && page.iter().any(|event| !older(event, previous))
        {
            return Err("Inbox message updates did not advance. Retry inbox.".into());
        }
        collected.extend(page.iter().cloned());
        let bytes: usize = collected.iter().map(Verified::byte_size).sum();
        if collected.len() > OVERLAY_EVENTS || bytes > OVERLAY_BYTES {
            return Err("Inbox message updates exceed the read budget. Retry inbox.".into());
        }
        // Authorized responses can have short pages; only empty ends it.
        let Some(last) = page.last() else {
            return Ok(collected);
        };
        cursor = Some((last.created_at_secs(), last.id_hex()));
    }
}

impl Actor {
    /// Admitted addressed messages: naming the viewer, in no channel or in
    /// one channel the viewer is a live member of.
    fn feed_project(&self, events: &[Verified]) -> Vec<String> {
        let viewer = self.identity.pubkey_hex();
        let channels = self
            .discovery
            .as_ref()
            .map(|discovery| discovery.channels())
            .unwrap_or_default();
        events
            .iter()
            .filter(|event| MENTION_KINDS.contains(&event.kind_u16()))
            .filter(|event| event.has_tag("p", &viewer))
            .filter(|event| {
                let hs: Vec<&[String]> = event.tags_named("h").collect();
                match hs.as_slice() {
                    [] => true,
                    [h] => h.get(1).is_some_and(|id| {
                        channels.iter().any(|channel| {
                            channel.id == *id
                                && !channel.cached
                                && channel.members.contains(&viewer)
                        })
                    }),
                    _ => false,
                }
            })
            .map(Verified::id_hex)
            .collect()
    }

    fn feed_publish(&mut self, feed: InboxFeed) {
        self.inbox.feed = feed;
        self.inbox_publish();
    }

    /// Republishes the inbox when the unread projection or the feed moved.
    pub(super) fn inbox_publish(&mut self) {
        if !self.inbox.wanted {
            return;
        }
        let Some(reads) = &mut self.reads else {
            return;
        };
        let inbox = reads.unread.inbox(&reads.sync);
        let can_read = reads.sync.snapshot().capability == ReadCapability::FrontierSync;
        let feed = self.inbox.feed.clone();
        self.inbox.sender.send_if_modified(|current| {
            if Arc::ptr_eq(&current.inbox, &inbox)
                && current.feed == feed
                && current.can_read == can_read
            {
                return false;
            }
            *current = Arc::new(InboxState {
                inbox: inbox.clone(),
                feed: feed.clone(),
                can_read,
            });
            true
        });
    }

    /// Reads the history unless a read is running (`refresh`).
    fn feed_refresh(&mut self) {
        if self.inbox.running {
            return;
        }
        self.inbox.requested = true;
        self.inbox.running = true;
        let epoch = self.inbox.epoch;
        let mut feed = self.inbox.feed.clone();
        feed.status = InboxFeedStatus::Loading;
        feed.error = None;
        self.feed_publish(feed);
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        let viewer = self.identity.pubkey_hex();
        self.runtime.spawn(async move {
            let mentions = reader
                .read(
                    vec![mentions_filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await;
            let (reply, targets) = oneshot::channel();
            let failed = mentions.as_ref().err().map(|error| error.message.clone());
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if actor.inbox.epoch != epoch {
                    let _ = reply.send(None);
                    return;
                }
                let events = match mentions {
                    Ok(events) => events,
                    Err(_) => {
                        let _ = reply.send(Some(Vec::new()));
                        return;
                    }
                };
                // Admission makes them unread evidence; the exact ids wait
                // for their edits and deletions.
                let visible = actor.accept(&events);
                actor.publish();
                let ids = actor.feed_project(&visible);
                let mut feed = actor.inbox.feed.clone();
                for id in ids {
                    if !feed.incomplete.contains(&id) {
                        feed.incomplete.push(id);
                    }
                }
                let targets = feed.incomplete.clone();
                actor.feed_publish(feed);
                let _ = reply.send(Some(targets));
            })));
            let Ok(Some(targets)) = targets.await else {
                return;
            };
            let result = match failed {
                Some(error) => Err(error),
                None => feed_overlays(&reader, &commands, targets).await,
            };
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if actor.inbox.epoch != epoch {
                    return;
                }
                actor.inbox.running = false;
                let mut feed = actor.inbox.feed.clone();
                match result {
                    Ok(()) => {
                        feed.status = InboxFeedStatus::Ready;
                        feed.incomplete.clear();
                        feed.error = None;
                    }
                    Err(error) => {
                        feed.status = InboxFeedStatus::Error;
                        feed.error = Some(error);
                    }
                }
                actor.feed_publish(feed);
            })));
        });
    }

    /// The connection dropped: stop the read and keep only the incomplete
    /// ids still retained.
    pub(super) fn inbox_stale(&mut self) {
        self.inbox.epoch += 1;
        self.inbox.running = false;
        let retained: Vec<String> = match &self.reads {
            Some(reads) => self
                .inbox
                .feed
                .incomplete
                .iter()
                .filter(|id| reads.unread.event(id).is_some())
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        self.feed_publish(InboxFeed {
            status: InboxFeedStatus::Idle,
            error: None,
            incomplete: retained,
        });
    }

    /// Back online: the history is read again if it was asked for.
    pub(super) fn inbox_reconnect(&mut self) {
        if self.inbox.requested {
            self.feed_refresh();
        }
    }
}

/// Reads the edits and deletions of `targets`, then the deletions of
/// their edits, admitting all of them.
async fn feed_overlays(
    reader: &crate::reader::Reader,
    commands: &tokio::sync::mpsc::UnboundedSender<Message>,
    targets: Vec<String>,
) -> Result<(), String> {
    let updates = overlays(
        reader,
        &targets,
        &[kinds::EDIT, kinds::DELETION, kinds::MODERATOR_DELETE],
    )
    .await?;
    let edits: Vec<String> = updates
        .iter()
        .filter(|event| event.kind_u16() == kinds::EDIT)
        .map(Verified::id_hex)
        .collect();
    let (reply, retained) = oneshot::channel();
    let probe = targets.clone();
    let _ = commands.send(Message::With(Box::new(move |actor| {
        actor.accept(&updates);
        actor.publish();
        let ids = actor
            .reads
            .as_mut()
            .map(|reads| reads.unread.retained_edit_ids(&probe))
            .unwrap_or_default();
        let _ = reply.send(ids);
    })));
    let retained = retained.await.map_err(|_| CLOSED.to_owned())?;
    let mut edit_ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for id in retained.into_iter().chain(edits) {
        if seen.insert(id.clone()) {
            edit_ids.push(id);
        }
    }
    let deletions = overlays(
        reader,
        &edit_ids,
        &[kinds::DELETION, kinds::MODERATOR_DELETE],
    )
    .await?;
    let (reply, done) = oneshot::channel();
    let _ = commands.send(Message::With(Box::new(move |actor| {
        actor.accept(&deletions);
        actor.publish();
        let _ = reply.send(());
    })));
    done.await.map_err(|_| CLOSED.to_owned())
}

impl Session {
    /// Follows the inbox, starting the history read once.
    pub fn inbox(&self) -> watch::Receiver<Arc<InboxState>> {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.inbox.wanted = true;
            if actor.inbox.feed.status == InboxFeedStatus::Idle {
                actor.feed_refresh();
            }
            actor.inbox_publish();
        })));
        self.inbox.clone()
    }

    /// The inbox as it is once followed, as `unread.inbox()` builds it
    /// on the spot: what [`Self::inbox`] published first.
    pub async fn inbox_now(&self) -> Result<watch::Receiver<Arc<InboxState>>, String> {
        let receiver = self.inbox();
        super::util::with(&self.commands, |_| ()).await?;
        Ok(receiver)
    }

    /// "Refresh" and "Retry inbox": unread evidence and the history again.
    pub fn refresh_inbox(&self) {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            actor.refresh_unread();
            actor.feed_refresh();
        })));
    }

    /// Reads an inbox row: its channel, or each of its thread prefixes in
    /// turn. Refused once the row is gone or changed.
    pub async fn read_inbox_item(&self, item: InboxItem) -> Result<(), String> {
        self.inbox_mark(item, false).await
    }

    /// Shows an inbox row as unread on this device (`markUnreadLocal`).
    pub async fn mark_inbox_item_unread(&self, item: InboxItem) -> Result<(), String> {
        self.inbox_mark(item, true).await
    }

    async fn inbox_mark(&self, item: InboxItem, unread: bool) -> Result<(), String> {
        super::util::with(&self.commands, move |actor| actor.inbox_mark(&item, unread)).await?
    }
}

impl Actor {
    fn inbox_mark(&mut self, item: &InboxItem, unread: bool) -> Result<(), String> {
        let channel_ok = self.discovery.as_ref().is_some_and(|discovery| {
            discovery.channels().iter().any(|channel| {
                channel.id == item.channel_id
                    && !channel.cached
                    && !channel.read_only
                    && !channel.archived
                    && channel.members.contains(&self.identity.pubkey_hex())
            })
        });
        let reads = self
            .reads
            .as_mut()
            .ok_or("Not connected to the relay yet")?;
        let present = reads.unread.inbox(&reads.sync).items.iter().any(|row| {
            row.channel_id == item.channel_id && row.message_ids.contains(&item.message_id)
        });
        if !channel_ok
            || !present
            || reads.sync.snapshot().capability != ReadCapability::FrontierSync
        {
            return Err(EXPIRED.into());
        }
        let now = super::unread::now_ms();
        let (sync, unread_state) = (&mut reads.sync, &mut reads.unread);
        let result = if unread {
            unread_state
                .mark_unread_local(sync, &item.target, now)
                .map(drop)
        } else if matches!(item.target, ReadTarget::Channel { .. }) {
            unread_state
                .mark_channel_read(sync, &item.channel_id, now / 1000, now)
                .map(drop)
        } else {
            item.read_through.iter().try_for_each(|step| {
                unread_state
                    .mark_through(sync, &step.target, &step.message_id, now)
                    .map(drop)
            })
        };
        self.publish();
        result.map_err(|error| error.to_string())
    }
}
