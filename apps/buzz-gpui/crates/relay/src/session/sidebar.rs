//! The sidebar's session-owned state (`sidebar-preferences-store.ts`,
//! `native-sidebar.ts`, `useHiddenDms.ts`): the kind-30078 preferences
//! with their serialized write queue, and the DMs removed from Messages
//! on this device.
//!
//! Preference writes run one at a time on a queue task: each reads the
//! strong head, signs the edited record, publishes it and reads it back.
//! Results return to the actor through [`Message::With`], fenced by the
//! store's tickets. The sidebar sees everything in
//! [`SessionSnapshot::sidebar`](super::SessionSnapshot::sidebar).

use super::util::CLOSED;
use super::{Actor, Message, Session};
use crate::ChannelType;
use crate::event::Verified;
use crate::http::RelayHttp;
use crate::kit::GroupsId;
use crate::kit::me::{self, RawKitEntry};
use crate::read_state::ReadTarget;
use crate::reader::{Priority, ReadOptions, Reader};
use crate::sidebar::{
    AssignmentIntent, HIDDEN_DM_PROBE_ATTEMPTS, HIDDEN_DMS_VIEW_KEY, LocalHiddenDms, MessageHead,
    SidebarHead, SidebarPreferences, SidebarWrite, SortMode, WriteResult, decode_records,
    head_filter, hidden_dm_probe_backoff_ms, next_created_at, preference_filters,
    project_preferences, record_template,
};
use crate::sidebar_store::{Destination, MoveIntent, PreferencesStore, SidebarState, Ticket};
use crate::store::Partition;
use crate::user_status::{self, UserStatus};
use buzz_identity::{Identity, Purpose};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The startup read and every write step give up after this.
const TIMEOUT: Duration = Duration::from_secs(10);
/// View state key of the preferences shown before the first read.
const STARTUP_KEY: &str = "sidebar-preferences-startup";

pub(super) struct SidebarPrefs {
    store: PreferencesStore,
    queue: mpsc::UnboundedSender<BoxFuture<'static, ()>>,
    /// A read was asked for while moves were saving.
    read_after_writes: bool,
    hidden: LocalHiddenDms,
    /// Hidden ids and DM roster of the last probe pass.
    probed: Option<String>,
    probing: bool,
    state: Arc<SidebarState>,
    /// The Me placement parsed from these catalog entries.
    me_channels: Option<(Arc<[RawKitEntry]>, Vec<String>)>,
}

impl SidebarPrefs {
    /// Whether `channel` is muted, as last confirmed by the relay; `None`
    /// until the preferences were read (`messages.ts` waits then).
    pub(super) fn muted(&self, channel: &str) -> Option<bool> {
        self.store
            .confirmed()
            .map(|preferences| preferences.muted.iter().any(|id| id == channel))
    }

    pub(super) fn new(runtime: &tokio::runtime::Handle) -> Self {
        let (queue, mut jobs) = mpsc::unbounded_channel::<BoxFuture<'static, ()>>();
        runtime.spawn(async move {
            while let Some(job) = jobs.recv().await {
                job.await;
            }
        });
        Self {
            store: PreferencesStore::default(),
            queue,
            read_after_writes: false,
            hidden: LocalHiddenDms::default(),
            probed: None,
            probing: false,
            state: Arc::default(),
            me_channels: None,
        }
    }

    pub(super) fn snapshot(&self) -> Arc<SidebarState> {
        self.state.clone()
    }
}

/// What the network steps need, cloned out of the actor.
#[derive(Clone)]
struct Writer {
    reader: Reader,
    http: RelayHttp,
    identity: Arc<Identity>,
    viewer: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn timed<T>(
    work: impl Future<Output = Result<T, String>>,
    message: &str,
) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(message.to_owned()))
}

impl Writer {
    /// The four records, decoded (`readSidebarPreferences`).
    async fn read(&self) -> Result<SidebarPreferences, String> {
        let events = self
            .reader
            .read(preference_filters(&self.viewer), ReadOptions::default())
            .await
            .map_err(|error| format!("Preference query: {}", error.message))?;
        decode_records(&self.identity, &events)
            .and_then(|decoded| decoded.preferences())
            .map_err(|error| format!("Preference decode: {error}"))
    }

    async fn head(&self, write: &SidebarWrite) -> Result<SidebarHead, String> {
        let coordinate = write.coordinate();
        let events = self
            .reader
            .read(
                vec![head_filter(&self.viewer, coordinate)],
                ReadOptions {
                    fresh: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.message)?;
        if events.is_empty() {
            return Ok(SidebarHead::empty(coordinate));
        }
        let decoded = decode_records(&self.identity, &events).map_err(|e| e.to_string())?;
        SidebarHead::from_decoded(coordinate, &decoded).map_err(|e| e.to_string())
    }

    /// Head, edit, sign, publish, read back (`native-sidebar.ts` `mutate`).
    async fn write(&self, write: SidebarWrite) -> Result<WriteResult, String> {
        write.validate().map_err(|e| e.to_string())?;
        let work = async {
            let head = self.head(&write).await?;
            let prepared = write.prepare(&head, now_ms()).map_err(|e| e.to_string())?;
            let Some(next) = prepared.next else {
                return Ok(prepared.result);
            };
            let created_at = next_created_at(now_ms() / 1000, head.created_at);
            let unsigned = record_template(&self.identity, write.coordinate(), &next, created_at)
                .map_err(|e| e.to_string())?;
            let event = self
                .identity
                .sign(Purpose::AppData, unsigned)
                .map_err(|e| e.to_string())?;
            super::util::publish(&self.http, &event).await?;
            let readback = self.head(&write).await?;
            if !write.confirms(&readback).map_err(|e| e.to_string())? {
                return Err(write.conflict().to_string());
            }
            write
                .prepare(&readback, now_ms())
                .map(|prepared| prepared.result)
                .map_err(|e| e.to_string())
        };
        timed(work, "Saving the sidebar timed out. Try again.").await
    }

    /// Assignment (or creation) before the star change, as the reference
    /// orders a move.
    async fn moved(
        &self,
        intent: &MoveIntent,
    ) -> Result<(Option<crate::sidebar::SidebarGroups>, Vec<String>), String> {
        let channel_id = intent.channel_id.clone();
        let starring = intent.destination == Destination::Starred;
        let groups = match &intent.destination {
            Destination::Starred => None,
            Destination::Section(section) => Some(AssignmentIntent {
                channel_id: channel_id.clone(),
                section_id: section.clone(),
                create_section: None,
            }),
            Destination::Create(new) => Some(AssignmentIntent {
                channel_id: channel_id.clone(),
                section_id: None,
                create_section: Some(new.clone()),
            }),
        };
        let groups = match groups {
            Some(intent) => match self.write(SidebarWrite::Assign(intent)).await? {
                WriteResult::Groups(groups) => Some(groups),
                _ => None,
            },
            None => None,
        };
        let starred = match self
            .write(SidebarWrite::Star {
                channel_id,
                starred: starring,
            })
            .await?
        {
            WriteResult::Starred(starred) => starred,
            _ => Vec::new(),
        };
        Ok((groups, starred))
    }
}

/// Preferences as the startup copy stores them (`writeStartup`).
fn startup_json(data: &SidebarPreferences) -> Value {
    json!({
        "sections": data.sections.iter().map(|s| {
            let mut entry = json!({"id": s.id, "name": s.name, "order": s.order});
            if let Some(icon) = &s.icon {
                entry["icon"] = json!(icon);
            }
            entry
        }).collect::<Vec<_>>(),
        "assignments": data.assignments,
        "starred": data.starred,
        "muted": data.muted,
        "sort": data.sort.iter().map(|(k, v)| (k.clone(), json!(v.as_str()))).collect::<Map<_, _>>(),
    })
}

/// Projects a startup copy through the record validation, as `readStartup`
/// does; anything malformed is dropped.
fn parse_startup(value: &Value) -> Option<SidebarPreferences> {
    let rows = |key: &str, flag: &str| {
        let channels: Map<String, Value> = value
            .get(key)?
            .as_array()?
            .iter()
            .map(|id| Some((id.as_str()?.to_owned(), json!({flag: true, "updatedAt": 0}))))
            .collect::<Option<_>>()?;
        Some(json!({"version": 1, "channels": channels}))
    };
    let sections = json!({
        "version": 1,
        "sections": value.get("sections")?,
        "assignments": value.get("assignments")?,
    });
    let sort = json!({"version": 1, "groups": value.get("sort")?});
    project_preferences(
        Some(&sections),
        rows("starred", "starred").as_ref(),
        rows("muted", "muted").as_ref(),
        Some(&sort),
        None,
    )
    .ok()
}

impl Session {
    async fn sidebar_call<T: Send + 'static>(
        &self,
        start: impl FnOnce(&mut Actor, oneshot::Sender<Result<T, String>>) + Send + 'static,
    ) -> Result<T, String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Message::With(Box::new(move |actor| start(actor, reply))))
            .map_err(|_| CLOSED.to_owned())?;
        answer.await.unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }

    /// Reads the sidebar preferences again (Retry).
    pub fn refresh_sidebar(&self) {
        let _ = self
            .commands
            .send(Message::With(Box::new(|actor| actor.sidebar_read())));
    }

    /// Moves a channel to Starred, a group, its default section or a new
    /// group. Shown at once; resolves once the relay confirms it. A failure
    /// shows in [`SidebarState::moves`] until retried or dismissed.
    pub async fn sidebar_move(
        &self,
        channel: &str,
        destination: Destination,
    ) -> Result<(), String> {
        let channel = channel.to_owned();
        self.sidebar_call(move |actor, reply| {
            let started = actor.sidebar.store.begin_move(&channel, destination);
            actor.sidebar_move(started, reply);
        })
        .await
    }

    pub async fn sidebar_retry_move(&self, channel: &str) -> Result<(), String> {
        let channel = channel.to_owned();
        self.sidebar_call(move |actor, reply| {
            let started = actor.sidebar.store.retry_move(&channel);
            actor.sidebar_move(started, reply);
        })
        .await
    }

    pub fn sidebar_dismiss_move(&self, channel: &str) {
        let channel = channel.to_owned();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.sidebar.store.dismiss_move(&channel);
            actor.publish();
        })));
    }

    /// Sorts a section (`group` is its sort key). Shown at once.
    pub async fn sidebar_sort(&self, group: &str, mode: SortMode) -> Result<(), String> {
        let group = group.to_owned();
        self.sidebar_call(move |actor, reply| {
            let ticket = match actor.sidebar.store.begin_sort(&group, mode) {
                Ok(ticket) => ticket,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let section_ids = actor
                .sidebar
                .store
                .confirmed()
                .map(|data| data.sections.iter().map(|s| s.id.clone()).collect())
                .unwrap_or_default();
            actor.publish();
            let writer = actor.sidebar_writer();
            let commands = actor.commands.clone();
            actor.sidebar_queue(async move {
                let result = writer
                    .write(SidebarWrite::Sort {
                        group,
                        mode,
                        section_ids,
                    })
                    .await
                    .and_then(|result| match result {
                        WriteResult::Sort(sort) => Ok(sort),
                        _ => Err("Invalid sidebar sort".into()),
                    });
                let _ = commands.send(Message::With(Box::new(move |actor| {
                    let outcome = result.as_ref().map(drop).map_err(Clone::clone);
                    actor.sidebar.store.finish_sort(ticket, result);
                    actor.publish();
                    let _ = reply.send(outcome);
                })));
            });
        })
        .await
    }

    pub fn sidebar_dismiss_sort(&self, group: &str) {
        let group = group.to_owned();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.sidebar.store.dismiss_sort(&group);
            actor.publish();
        })));
    }

    /// Mutes or unmutes a channel. Not optimistic here: the sidebar shows
    /// the intent while it saves.
    pub async fn sidebar_mute(&self, channel: &str, muted: bool) -> Result<(), String> {
        let channel_id = channel.to_owned();
        self.sidebar_call(move |actor, reply| {
            let ticket = match actor.sidebar.store.begin_mute() {
                Ok(ticket) => ticket,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let writer = actor.sidebar_writer();
            let commands = actor.commands.clone();
            actor.sidebar_queue(async move {
                let result = writer
                    .write(SidebarWrite::Mute { channel_id, muted })
                    .await
                    .and_then(|result| match result {
                        WriteResult::Muted(muted) => Ok(muted),
                        _ => Err("Invalid sidebar mutes".into()),
                    });
                let _ = commands.send(Message::With(Box::new(move |actor| {
                    let outcome =
                        result.and_then(|muted| actor.sidebar.store.finish_mute(ticket, muted));
                    actor.publish();
                    // Held alerts follow the mutes.
                    actor.pump_incoming();
                    let _ = reply.send(outcome);
                })));
            });
        })
        .await
    }

    /// Removes a saved group: its sort goes back to A–Z, then the group
    /// is tombstoned and its channels return to their default sections.
    pub async fn sidebar_remove_section(&self, section: &str) -> Result<(), String> {
        let section = section.to_owned();
        self.sidebar_call(move |actor, reply| {
            let (ticket, section_ids) = match actor.sidebar.store.begin_remove_section(&section) {
                Ok(started) => started,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let writer = actor.sidebar_writer();
            let commands = actor.commands.clone();
            actor.sidebar_queue(async move {
                let sort = writer
                    .write(SidebarWrite::Sort {
                        group: format!("section:{section}"),
                        mode: SortMode::Alpha,
                        section_ids,
                    })
                    .await;
                let (sort, groups) = match sort {
                    Ok(WriteResult::Sort(sort)) => {
                        let groups = writer
                            .write(SidebarWrite::RemoveSection {
                                section_id: section,
                            })
                            .await
                            .and_then(|result| match result {
                                WriteResult::Groups(groups) => Ok(groups),
                                _ => Err("Invalid sidebar groups".into()),
                            });
                        (Some(sort), groups)
                    }
                    Ok(_) => (None, Err("Invalid sidebar sort".into())),
                    Err(error) => (None, Err(error)),
                };
                let _ = commands.send(Message::With(Box::new(move |actor| {
                    let outcome = actor
                        .sidebar
                        .store
                        .finish_remove_section(ticket, sort, groups);
                    actor.publish();
                    let _ = reply.send(outcome);
                })));
            });
        })
        .await
    }

    /// "Remove from Messages": hides a DM on this device until a new
    /// message arrives in it, the viewer sends there or opens it again.
    pub fn hide_dm(&self, channel: &str) {
        let channel = channel.to_owned();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            let latest = actor.latest_head(&channel);
            actor.sidebar.hidden.hide(&channel, latest);
            actor.sidebar_hidden_changed();
        })));
    }

    /// Current statuses of these people (at most 100 per read), for the
    /// sidebar's DM rows. People without a status are absent.
    pub async fn user_statuses(
        &self,
        people: Vec<String>,
    ) -> Result<HashMap<String, UserStatus>, String> {
        let reader = self.direct.reader.clone();
        self.run(async move {
            let mut statuses = HashMap::new();
            let now = now_ms() / 1000;
            for batch in people.chunks(100) {
                let mut filter = user_status::filter("");
                filter.insert("authors".into(), json!(batch));
                filter.insert("limit".into(), json!(batch.len()));
                let events = reader
                    .read(vec![filter], ReadOptions::background())
                    .await
                    .map_err(|error| error.message)?;
                for person in batch {
                    if let Some(status) = user_status::current(&events, person, now) {
                        statuses.insert(person.clone(), status);
                    }
                }
            }
            Ok(statuses)
        })
        .await
    }
}

impl Actor {
    fn sidebar_writer(&self) -> Writer {
        Writer {
            reader: self.reader.clone(),
            http: self.http.clone(),
            identity: self.identity.clone(),
            viewer: self.identity.pubkey_hex(),
        }
    }

    fn sidebar_queue(&self, job: impl Future<Output = ()> + Send + 'static) {
        let _ = self.sidebar.queue.send(Box::pin(job));
    }

    pub(super) fn partition(&self) -> Partition {
        Partition::new(&self.origin, &self.identity.pubkey_hex())
    }

    /// Restores the local state saved for this community and viewer.
    pub(super) fn sidebar_restore(&mut self) {
        let partition = self.partition();
        let saved = |key: &str| {
            self.store
                .view_state(&partition, key)
                .ok()
                .flatten()
                .and_then(|json| serde_json::from_str::<Value>(&json).ok())
        };
        if let Some(hidden) = saved(HIDDEN_DMS_VIEW_KEY) {
            self.sidebar.hidden = LocalHiddenDms::restore(&hidden);
        }
        if let Some(startup) = saved(STARTUP_KEY)
            && let Some(saved_at) = startup.get("savedAt").and_then(Value::as_u64)
            && crate::sidebar::startup_cache_usable(saved_at, now_ms())
            && let Some(data) = startup.get("data").and_then(parse_startup)
        {
            self.sidebar.store.restore_cached(data);
        }
        self.sidebar_state();
    }

    /// Recomputes the published sidebar state.
    pub(super) fn sidebar_state(&mut self) {
        let mut next = self
            .sidebar
            .store
            .state(self.sidebar.hidden.ids().collect());
        // Parse the Me record again only when the catalog entries change.
        let entries = self.me_kit.snapshot().entries.clone();
        next.me_channels = match &self.sidebar.me_channels {
            Some((parsed, ids)) if Arc::ptr_eq(parsed, &entries) => ids.clone(),
            _ => {
                let ids = me::groups(&entries, GroupsId::Me)
                    .map(|groups| groups.channels)
                    .unwrap_or_default();
                self.sidebar.me_channels = Some((entries, ids.clone()));
                ids
            }
        };
        if *self.sidebar.state != next {
            self.sidebar.state = Arc::new(next);
        }
    }

    /// Reads the preferences, now or once the saving moves finish.
    pub(super) fn sidebar_read(&mut self) {
        if self.sidebar.store.has_pending_moves() {
            self.sidebar.read_after_writes = true;
            return;
        }
        let Some(ticket) = self.sidebar.store.begin_read() else {
            return;
        };
        self.publish();
        let writer = self.sidebar_writer();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let result = timed(writer.read(), "Preference query: timed out").await;
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if let Ok(data) = &result
                    && actor.sidebar.store.finish_read(ticket, Ok(data.clone()))
                {
                    actor.save_sidebar_startup(data);
                } else if let Err(error) = result {
                    actor.sidebar.store.finish_read(ticket, Err(error));
                }
                actor.publish();
                // Alerts other than mentions wait on the mutes.
                actor.pump_incoming();
            })));
        });
    }

    /// The first read once the relay identified itself.
    pub(super) fn sidebar_connected(&mut self) {
        if self.sidebar.store.confirmed().is_none() {
            self.sidebar_read();
        }
    }

    fn save_sidebar_startup(&self, data: &SidebarPreferences) {
        let value = json!({"savedAt": now_ms(), "data": startup_json(data)}).to_string();
        let (store, partition) = (self.store.clone(), self.partition());
        self.disk.write(move || {
            if let Err(error) = store.set_view_state(&partition, STARTUP_KEY, Some(&value)) {
                tracing::warn!(%error, "could not save the sidebar preferences");
            }
        });
    }

    fn sidebar_move(
        &mut self,
        started: Result<(Ticket, MoveIntent), String>,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        let (ticket, intent) = match started {
            Ok(started) => started,
            Err(error) => {
                self.publish();
                let _ = reply.send(Err(error));
                return;
            }
        };
        self.publish();
        let writer = self.sidebar_writer();
        let commands = self.commands.clone();
        self.sidebar_queue(async move {
            let result = writer.moved(&intent).await;
            let _ = commands.send(Message::With(Box::new(move |actor| {
                let outcome = actor.sidebar.store.finish_move(ticket, result);
                if let Ok(()) = &outcome
                    && let Some(data) = actor.sidebar.store.confirmed().cloned()
                {
                    actor.save_sidebar_startup(&data);
                }
                if !actor.sidebar.store.has_pending_moves()
                    && std::mem::take(&mut actor.sidebar.read_after_writes)
                {
                    actor.sidebar_read();
                }
                actor.publish();
                let _ = reply.send(outcome);
            })));
        });
    }

    // ----- hidden DMs -----

    /// The newest message in a channel, as unread tracking saw it.
    fn latest_head(&mut self, channel: &str) -> Option<MessageHead> {
        let reads = self.reads.as_mut()?;
        reads
            .unread
            .snapshot(&ReadTarget::channel(channel), &reads.sync)
            .latest_message
            .as_ref()
            .map(|latest| MessageHead {
                id: latest.id.clone(),
                created_at: latest.created_at,
            })
    }

    fn save_hidden_dms(&self) {
        let json = self.sidebar.hidden.to_json().to_string();
        let (store, partition) = (self.store.clone(), self.partition());
        self.disk.write(move || {
            if let Err(error) = store.set_view_state(&partition, HIDDEN_DMS_VIEW_KEY, Some(&json)) {
                tracing::warn!(%error, "could not save the hidden conversations");
            }
        });
    }

    fn sidebar_hidden_changed(&mut self) {
        self.save_hidden_dms();
        self.sidebar_probe();
        self.publish();
    }

    /// Shows hidden DMs again: a live message from someone else arrived.
    pub(super) fn sidebar_arrived(&mut self, channels: &[String]) {
        let ids: Vec<&str> = channels.iter().map(String::as_str).collect();
        if self.sidebar.hidden.show(&ids) {
            self.sidebar_hidden_changed();
        }
    }

    /// The hide generation a send starts under, to show the DM after it.
    pub(super) fn sidebar_sending(&self, channel: &str) -> Option<(String, u64)> {
        self.sidebar
            .hidden
            .generation_of(channel)
            .map(|generation| (channel.to_owned(), generation))
    }

    pub(super) fn sidebar_sent(&mut self, sending: Option<(String, u64)>) {
        if let Some((channel, generation)) = sending
            && self.sidebar.hidden.sent(&channel, generation)
        {
            self.sidebar_hidden_changed();
        }
    }

    /// Before each publish: DMs whose newest message moved past the hide
    /// baseline come back.
    pub(super) fn sidebar_check_heads(&mut self) {
        if self.sidebar.hidden.entries().is_empty() || self.reads.is_none() {
            return;
        }
        let ids: Vec<String> = self.sidebar.hidden.ids().collect();
        let heads: HashMap<String, Option<MessageHead>> = ids
            .into_iter()
            .map(|id| {
                let head = self.latest_head(&id);
                (id, head)
            })
            .collect();
        if self
            .sidebar
            .hidden
            .check_heads(|id| heads.get(id).cloned().flatten())
        {
            let json = self.sidebar.hidden.to_json().to_string();
            let _ = self
                .store
                .set_view_state(&self.partition(), HIDDEN_DMS_VIEW_KEY, Some(&json));
        }
    }

    /// A bounded history read per hidden DM in the roster: the first one
    /// records what was there, later ones show the DM on anything new.
    pub(super) fn sidebar_probe(&mut self) {
        if self.sidebar.probing || self.sidebar.hidden.entries().is_empty() {
            return;
        }
        let Some(discovery) = &self.discovery else {
            return;
        };
        let mut dms: Vec<String> = discovery
            .channels()
            .into_iter()
            .filter(|c| c.channel_type == Some(ChannelType::Dm))
            .map(|c| c.id)
            .collect();
        dms.sort();
        let hidden: Vec<String> = self.sidebar.hidden.ids().collect();
        let key = format!("{}|{}", hidden.join(","), dms.join(","));
        if self.sidebar.probed.as_ref() == Some(&key) {
            return;
        }
        self.sidebar.probed = Some(key);
        let probes: Vec<_> = self
            .sidebar
            .hidden
            .entries()
            .iter()
            .filter(|entry| dms.binary_search(&entry.id).is_ok())
            .filter_map(|entry| {
                let filter = self.sidebar.hidden.probe_filter(&entry.id)?;
                Some((entry.id.clone(), entry.generation, filter))
            })
            .collect();
        if probes.is_empty() {
            return;
        }
        self.sidebar.probing = true;
        let (reader, commands) = (self.reader.clone(), self.commands.clone());
        self.runtime.spawn(async move {
            for (id, generation, filter) in probes {
                for attempt in 0..HIDDEN_DM_PROBE_ATTEMPTS {
                    let options = ReadOptions {
                        priority: Priority::Background,
                        fresh: false,
                    };
                    match reader.read(vec![filter.clone()], options).await {
                        Ok(events) => {
                            let ids: Vec<String> = events.iter().map(Verified::id_hex).collect();
                            let id = id.clone();
                            let _ = commands.send(Message::With(Box::new(move |actor| {
                                let latest = actor.latest_head(&id);
                                let outcome = actor
                                    .sidebar
                                    .hidden
                                    .apply_probe(&id, generation, &ids, latest);
                                if outcome != crate::sidebar::ProbeOutcome::Unchanged {
                                    actor.save_hidden_dms();
                                    actor.publish();
                                }
                            })));
                            break;
                        }
                        Err(_) if attempt + 1 < HIDDEN_DM_PROBE_ATTEMPTS => {
                            let wait = hidden_dm_probe_backoff_ms(attempt);
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                        }
                        Err(_) => {}
                    }
                }
            }
            let _ = commands.send(Message::With(Box::new(|actor| {
                actor.sidebar.probing = false;
                actor.sidebar_probe();
            })));
        });
    }

    /// The DM picker opened (or reopened) this conversation.
    pub(super) fn sidebar_opened(&mut self, channel: &str) {
        if self.sidebar.hidden.show(&[channel]) {
            self.sidebar_hidden_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::SavedSection;

    #[test]
    fn startup_copies_round_trip_through_validation() {
        let data = SidebarPreferences {
            sections: vec![SavedSection {
                id: "s1".into(),
                name: "Work".into(),
                icon: Some("🔥".into()),
                order: 0.0,
            }],
            assignments: [("c1".to_string(), "s1".to_string())].into(),
            starred: vec!["c2".into()],
            muted: vec!["c3".into()],
            sort: [("channels".to_string(), SortMode::Recent)].into(),
        };
        let parsed = parse_startup(&startup_json(&data)).unwrap();
        assert_eq!(parsed.sections, data.sections);
        assert_eq!(parsed.assignments, data.assignments);
        assert_eq!(parsed.starred, data.starred);
        assert_eq!(parsed.muted, data.muted);
        assert_eq!(parsed.sort, data.sort);
        assert!(parse_startup(&json!({"sections": 3})).is_none());
    }
}
