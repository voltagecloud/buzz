//! The session's reminders (`session.ts` `createReminders`): the list read,
//! live arrivals on the membership route, recovery after a reconnect, and
//! create, snooze, complete and cancel through a direct publish.

use super::util::{CLOSED, now, publish, with};
use super::{Actor, Message, Session};
use crate::event::{Verified, kinds};
use crate::reader::ReadOptions;
use crate::reminders::{
    RawReminder, Reminder, ReminderEdit, ReminderStore, ReminderTarget, RemindersState,
    create_intent, edit_intent, list_filter, random_d,
};
use buzz_identity::{DecodedReminder, Identity};
use nostr::prelude::Event;
use std::sync::Arc;
use tokio::sync::watch;

/// The actor's side.
pub(super) struct Owner {
    store: ReminderStore,
    sender: watch::Sender<Arc<RemindersState>>,
    /// Someone watches the list, so reads are worth making.
    wanted: bool,
}

/// The session's side.
pub(super) struct Shared {
    state: watch::Receiver<Arc<RemindersState>>,
    /// Edits run one after another, each on the previous result.
    writes: tokio::sync::Mutex<()>,
}

pub(super) fn new() -> (Owner, Arc<Shared>) {
    let (sender, state) = watch::channel(Arc::new(RemindersState::default()));
    (
        Owner {
            store: ReminderStore::default(),
            sender,
            wanted: false,
        },
        Arc::new(Shared {
            state,
            writes: tokio::sync::Mutex::new(()),
        }),
    )
}

fn raw(event: &Event) -> RawReminder {
    let tag = |name: &str| {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .find(|tag| tag.first().map(String::as_str) == Some(name))
            .and_then(|tag| tag.get(1).cloned())
    };
    RawReminder {
        event_id: event.id.to_hex(),
        created_at: event.created_at.as_secs(),
        d: tag("d"),
        not_before: tag("not_before"),
    }
}

/// Opens the viewer's own reminders among `events`.
fn decode(
    identity: &Identity,
    viewer: &str,
    events: &[Event],
) -> Result<(Vec<RawReminder>, Vec<DecodedReminder>), String> {
    let own: Vec<Event> = events
        .iter()
        .filter(|event| event.kind.as_u16() == kinds::REMINDERS && event.pubkey.to_hex() == viewer)
        .cloned()
        .collect();
    if own.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let decoded = identity
        .decode_reminders(&own)
        .map_err(|error| error.to_string())?;
    Ok((own.iter().map(raw).collect(), decoded))
}

impl Actor {
    fn reminders_publish(&self) {
        let state = self.reminders.store.state();
        self.reminders.sender.send_if_modified(|current| {
            if **current == *state {
                return false;
            }
            *current = Arc::new(state.clone());
            true
        });
    }

    /// Reads the list unless a read for this generation is in flight.
    pub(super) fn reminders_refresh(&mut self) {
        self.reminders.wanted = true;
        if let Some(generation) = self.reminders.store.begin_read() {
            self.reminders_read(generation);
        }
        self.reminders_publish();
    }

    fn reminders_read(&self, generation: u64) {
        let reader = self.reader.clone();
        let identity = self.identity.clone();
        let viewer = self.identity.pubkey_hex();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let result = match reader
                .read(
                    vec![list_filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(events) => {
                    let events: Vec<Event> =
                        events.iter().map(|event| event.event().clone()).collect();
                    decode(&identity, &viewer, &events)
                }
                Err(error) => Err(error.message),
            };
            let _ = commands.send(Message::With(Box::new(move |actor| {
                actor.reminders.store.read_done(generation, result);
                actor.reminders_publish();
            })));
        });
    }

    /// Live 30300s from another device. Failures keep the last good list.
    pub(super) fn reminders_live(&mut self, events: &[Verified]) {
        let events: Vec<Event> = events
            .iter()
            .filter(|event| event.kind_u16() == kinds::REMINDERS)
            .map(|event| event.event().clone())
            .collect();
        if events.is_empty() {
            return;
        }
        let viewer = self.identity.pubkey_hex();
        if let Ok((raw, decoded)) = decode(&self.identity, &viewer, &events)
            && self.reminders.store.received(&raw, &decoded)
        {
            self.reminders_publish();
        }
    }

    /// The connection dropped.
    pub(super) fn reminders_stale(&mut self) {
        self.reminders.store.stale();
        self.reminders_publish();
    }

    /// The connection is up again: the read that ends the outage.
    pub(super) fn reminders_recover(&mut self) {
        let wanted = self.reminders.wanted;
        if let Some(generation) = self.reminders.store.recover(wanted) {
            self.reminders_read(generation);
        }
        self.reminders_publish();
    }
}

impl Session {
    /// Follows the viewer's reminders, reading the list on first use.
    pub fn reminders(&self) -> watch::Receiver<Arc<RemindersState>> {
        let _ = self.commands.send(Message::With(Box::new(|actor| {
            if !actor.reminders.wanted {
                actor.reminders_refresh();
            }
        })));
        self.reminders.state.clone()
    }

    /// Reads the list again ("Retry reminders").
    pub fn refresh_reminders(&self) {
        let _ = self
            .commands
            .send(Message::With(Box::new(|actor| actor.reminders_refresh())));
    }

    /// A new pending reminder for a message.
    pub async fn create_reminder(
        &self,
        target: ReminderTarget,
        not_before: u64,
        note: Option<String>,
    ) -> Result<(), String> {
        let writer = self.reminder_writer();
        self.runtime
            .spawn(async move {
                let _guard = writer.shared.writes.lock().await;
                let intent =
                    create_intent(&random_d(), now(), &target, not_before, note.as_deref());
                writer.write(intent).await
            })
            .await
            .map_err(|_| CLOSED.to_owned())?
    }

    /// Moves a pending reminder to `not_before`.
    pub async fn snooze_reminder(&self, id: &str, not_before: u64) -> Result<(), String> {
        self.edit_reminder(id, ReminderEdit::Snooze(not_before))
            .await
    }

    pub async fn complete_reminder(&self, id: &str) -> Result<(), String> {
        self.edit_reminder(id, ReminderEdit::Complete).await
    }

    pub async fn cancel_reminder(&self, id: &str) -> Result<(), String> {
        self.edit_reminder(id, ReminderEdit::Cancel).await
    }

    async fn edit_reminder(&self, id: &str, edit: ReminderEdit) -> Result<(), String> {
        let writer = self.reminder_writer();
        let id = id.to_owned();
        self.runtime
            .spawn(async move {
                let _guard = writer.shared.writes.lock().await;
                let current = writer.head(id).await?;
                let days = 30 + u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 60;
                writer.write(edit_intent(&current, edit, now(), days)).await
            })
            .await
            .map_err(|_| CLOSED.to_owned())?
    }

    fn reminder_writer(&self) -> Writer {
        Writer {
            commands: self.commands.clone(),
            http: self.direct.http.clone(),
            identity: self.direct.identity.clone(),
            viewer: self.viewer.clone(),
            shared: self.reminders.clone(),
        }
    }
}

/// What a reminder write needs, owned, so it runs on the session's
/// runtime whatever calls it.
struct Writer {
    commands: tokio::sync::mpsc::UnboundedSender<Message>,
    http: crate::http::RelayHttp,
    identity: Arc<Identity>,
    viewer: String,
    shared: Arc<Shared>,
}

impl Writer {
    async fn head(&self, id: String) -> Result<Reminder, String> {
        with(&self.commands, move |actor| {
            actor.reminders.store.get(&id).cloned()
        })
        .await?
        .ok_or_else(|| "Reminder not found".to_owned())
    }

    /// Seals, publishes and shows one reminder record.
    async fn write(&self, intent: buzz_identity::ReminderIntent) -> Result<(), String> {
        let event = self
            .identity
            .seal_reminder(intent, now())
            .map_err(|error| error.to_string())?;
        publish(&self.http, &event).await?;
        let viewer = self.viewer.clone();
        with(&self.commands, move |actor| {
            if let Ok((raw, decoded)) =
                decode(&actor.identity, &viewer, std::slice::from_ref(&event))
                && actor.reminders.store.received(&raw, &decoded)
            {
                actor.reminders_publish();
            }
        })
        .await
    }
}
