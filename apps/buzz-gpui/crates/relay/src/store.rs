//! Local persistence: one SQLite file per install (spec §7.5).
//!
//! Everything a community writes is keyed by its partition, the relay
//! origin plus the viewer's pubkey, so leaving a community is one
//! transaction. The outbox needs `synchronous=FULL`: an intent must be on
//! disk before its event is signed, and signed bytes before they are sent.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Partition {
    /// `https://host[:port]`; the byte format must never change, or saved
    /// data is orphaned (spec §9.2 #16).
    pub origin: String,
    pub viewer: String,
}

impl Partition {
    pub fn new(origin: &crate::RelayOrigin, viewer: &str) -> Self {
        Self {
            origin: origin.http(),
            viewer: viewer.to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("local storage failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("local data is corrupt: {0}")]
    Corrupt(String),
}

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE outbox_ops (
  origin        TEXT NOT NULL,
  viewer        TEXT NOT NULL,
  event_id      TEXT NOT NULL CHECK(length(event_id)=64),
  kind          INTEGER NOT NULL,
  created_at    INTEGER NOT NULL,
  channel_id    TEXT,
  template_json TEXT NOT NULL,
  signed_json   TEXT,
  delivery      TEXT NOT NULL CHECK(delivery IN ('sending','accepted','unknown','failed','seen')),
  error         TEXT,
  recovery_key  TEXT CHECK(recovery_key IS NULL OR length(recovery_key) BETWEEN 1 AND 200),
  recovery_value TEXT,
  acknowledged  INTEGER NOT NULL DEFAULT 0,
  guarded       INTEGER NOT NULL DEFAULT 0,
  confirmed     INTEGER NOT NULL DEFAULT 0,
  enqueued_seq  INTEGER NOT NULL,
  bytes         INTEGER NOT NULL,
  updated_ms    INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer, event_id)
);
CREATE INDEX outbox_pending ON outbox_ops(origin, viewer, confirmed, enqueued_seq);
CREATE UNIQUE INDEX outbox_recovery ON outbox_ops(origin, viewer, recovery_key)
  WHERE recovery_key IS NOT NULL AND confirmed = 0;

CREATE TABLE head_events (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, channel_id TEXT NOT NULL,
  event_id TEXT NOT NULL, created_at INTEGER NOT NULL, kind INTEGER NOT NULL,
  raw_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer, channel_id, event_id)
);
CREATE TABLE head_meta (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, channel_id TEXT NOT NULL,
  saved_ms INTEGER NOT NULL, bytes INTEGER NOT NULL, meta_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer, channel_id)
);
CREATE TABLE startup_cache (
  origin TEXT NOT NULL, viewer TEXT NOT NULL,
  discovery_json TEXT, preferences_json TEXT, saved_ms INTEGER NOT NULL, bytes INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer)
);
CREATE TABLE read_state (
  origin TEXT NOT NULL, viewer TEXT NOT NULL,
  journal_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer)
);
CREATE TABLE view_state (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, key TEXT NOT NULL,
  value_json TEXT NOT NULL,
  updated_ms INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer, key)
);
CREATE TABLE partition_prefs (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, name TEXT NOT NULL, value_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer, name)
);
CREATE TABLE viewer_prefs (
  viewer TEXT NOT NULL, name TEXT NOT NULL, value_json TEXT NOT NULL,
  PRIMARY KEY (viewer, name)
);
CREATE TABLE device_prefs (name TEXT PRIMARY KEY, value_json TEXT NOT NULL);
"#;

/// Tables holding one community's data, cleared together on leave.
const PARTITION_TABLES: &[&str] = &[
    "outbox_ops",
    "head_events",
    "head_meta",
    "startup_cache",
    "read_state",
    "view_state",
    "partition_prefs",
];

/// Shared handle to the database. Operations are short; callers on async
/// tasks that do bulk work should move it to a blocking thread.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        }
        Self::setup(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "secure_delete", "ON")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                connection.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {}
            newer => {
                return Err(StoreError::Corrupt(format!(
                    "database schema {newer} is newer than this app understands"
                )));
            }
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Runs `f` with exclusive access to the connection.
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut connection = self.connection.lock().expect("store lock");
        f(&mut connection)
    }

    /// Runs a write of view state or preferences, which buzz-app keeps in
    /// localStorage: it commits without waiting for the disk
    /// (`synchronous=NORMAL` under WAL), so a power cut may lose it but a
    /// crash cannot. The UI thread saves these as the user moves around, and
    /// a full sync costs milliseconds each. Everything else, the outbox
    /// first, keeps `synchronous=FULL`.
    fn with_relaxed<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.with(|c| {
            c.pragma_update(None, "synchronous", "NORMAL")?;
            let result = f(c);
            let restored = c.pragma_update(None, "synchronous", "FULL");
            let value = result?;
            restored?;
            Ok(value)
        })
    }

    pub fn view_state(
        &self,
        partition: &Partition,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value_json FROM view_state WHERE origin = ?1 AND viewer = ?2 AND key = ?3",
                params![partition.origin, partition.viewer, key],
                |row| row.get(0),
            )
            .optional()?)
        })
    }

    /// `None` removes the key.
    pub fn set_view_state(
        &self,
        partition: &Partition,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with_relaxed(|c| {
            match value {
                Some(value) => c.execute(
                    "INSERT INTO view_state (origin, viewer, key, value_json, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (origin, viewer, key) DO UPDATE SET
                       value_json = excluded.value_json, updated_ms = excluded.updated_ms",
                    params![partition.origin, partition.viewer, key, value, now_ms()],
                )?,
                None => c.execute(
                    "DELETE FROM view_state WHERE origin = ?1 AND viewer = ?2 AND key = ?3",
                    params![partition.origin, partition.viewer, key],
                )?,
            };
            Ok(())
        })
    }

    /// Every saved view key starting with `prefix`, newest first, with
    /// its value; for listing composer drafts (`listDraftViews`).
    pub fn view_states(
        &self,
        partition: &Partition,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT key, value_json FROM view_state
                 WHERE origin = ?1 AND viewer = ?2 AND substr(key, 1, length(?3)) = ?3
                 ORDER BY updated_ms DESC, key",
            )?;
            let rows = statement
                .query_map(params![partition.origin, partition.viewer, prefix], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn partition_pref(
        &self,
        partition: &Partition,
        name: &str,
    ) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value_json FROM partition_prefs WHERE origin = ?1 AND viewer = ?2 AND name = ?3",
                params![partition.origin, partition.viewer, name],
                |row| row.get(0),
            )
            .optional()?)
        })
    }

    pub fn set_partition_pref(
        &self,
        partition: &Partition,
        name: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        self.with_relaxed(|c| {
            c.execute(
                "INSERT INTO partition_prefs (origin, viewer, name, value_json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (origin, viewer, name) DO UPDATE SET value_json = excluded.value_json",
                params![partition.origin, partition.viewer, name, value],
            )?;
            Ok(())
        })
    }

    pub fn viewer_pref(&self, viewer: &str, name: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value_json FROM viewer_prefs WHERE viewer = ?1 AND name = ?2",
                params![viewer, name],
                |row| row.get(0),
            )
            .optional()?)
        })
    }

    pub fn set_viewer_pref(&self, viewer: &str, name: &str, value: &str) -> Result<(), StoreError> {
        self.with_relaxed(|c| {
            c.execute(
                "INSERT INTO viewer_prefs (viewer, name, value_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT (viewer, name) DO UPDATE SET value_json = excluded.value_json",
                params![viewer, name, value],
            )?;
            Ok(())
        })
    }

    pub fn device_pref(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value_json FROM device_prefs WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?)
        })
    }

    pub fn set_device_pref(&self, name: &str, value: &str) -> Result<(), StoreError> {
        self.with_relaxed(|c| {
            c.execute(
                "INSERT INTO device_prefs (name, value_json) VALUES (?1, ?2)
                 ON CONFLICT (name) DO UPDATE SET value_json = excluded.value_json",
                params![name, value],
            )?;
            Ok(())
        })
    }

    /// Saves one channel's head (`persistence.ts` `write`): its signed
    /// events and their authors' profiles as wire JSON, re-verified when
    /// loaded. A head over the byte budget is not saved. Afterwards the
    /// whole cache, across every partition, keeps only the newest heads
    /// within [`MAX_SAVED_HEADS`], [`MAX_SAVED_BYTES`] and a day.
    pub fn save_head(
        &self,
        partition: &Partition,
        channel: &str,
        saved_ms: i64,
        events: &[crate::Verified],
        profiles: &[crate::Verified],
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let profiles = format!(
            "[{}]",
            profiles
                .iter()
                .map(|event| event.as_json())
                .collect::<Vec<_>>()
                .join(",")
        );
        let raw: Vec<String> = events.iter().map(|event| event.as_json()).collect();
        let bytes = profiles.len() + raw.iter().map(String::len).sum::<usize>() + channel.len();
        if bytes > MAX_SAVED_BYTES {
            return Ok(false);
        }
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM head_events WHERE origin = ?1 AND viewer = ?2 AND channel_id = ?3",
                params![partition.origin, partition.viewer, channel],
            )?;
            {
                let mut insert = tx.prepare(
                    "INSERT OR REPLACE INTO head_events
                       (origin, viewer, channel_id, event_id, created_at, kind, raw_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for (event, raw) in events.iter().zip(&raw) {
                    insert.execute(params![
                        partition.origin,
                        partition.viewer,
                        channel,
                        event.id_hex(),
                        event.created_at_secs() as i64,
                        i64::from(event.kind_u16()),
                        raw,
                    ])?;
                }
            }
            tx.execute(
                "INSERT INTO head_meta (origin, viewer, channel_id, saved_ms, bytes, meta_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (origin, viewer, channel_id) DO UPDATE SET
                   saved_ms = excluded.saved_ms, bytes = excluded.bytes, meta_json = excluded.meta_json",
                params![
                    partition.origin,
                    partition.viewer,
                    channel,
                    saved_ms,
                    bytes as i64,
                    profiles
                ],
            )?;
            trim_heads(&tx, now_ms)?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// This partition's saved heads, newest first, at most
    /// [`MAX_SAVED_HEADS`] and none older than a day (`persistence.ts`
    /// `read`).
    pub fn load_heads(
        &self,
        partition: &Partition,
        now_ms: i64,
    ) -> Result<Vec<SavedHead>, StoreError> {
        self.with(|c| {
            let metas: Vec<(String, i64, String)> = {
                let mut query = c.prepare(
                    "SELECT channel_id, saved_ms, meta_json FROM head_meta
                     WHERE origin = ?1 AND viewer = ?2 ORDER BY saved_ms DESC",
                )?;
                query
                    .query_map(params![partition.origin, partition.viewer], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<Result<_, _>>()?
            };
            let mut heads = Vec::new();
            for (channel, saved_ms, profiles) in metas {
                if heads.len() >= MAX_SAVED_HEADS {
                    break;
                }
                if now_ms - saved_ms >= MAX_SAVED_AGE_MS || saved_ms > now_ms {
                    continue;
                }
                let mut query = c.prepare(
                    "SELECT raw_json FROM head_events WHERE origin = ?1 AND viewer = ?2 AND channel_id = ?3
                     ORDER BY created_at DESC, event_id ASC",
                )?;
                let events = query
                    .query_map(params![partition.origin, partition.viewer, channel], |row| {
                        row.get::<_, String>(0)
                    })?
                    .map(|raw| {
                        raw.map(|raw| serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null))
                    })
                    .collect::<Result<Vec<serde_json::Value>, _>>()?;
                // A corrupt profile list is optional data; the history stays.
                let profiles = serde_json::from_str(&profiles).unwrap_or_default();
                heads.push(SavedHead {
                    channel,
                    saved_ms,
                    events,
                    profiles,
                });
            }
            Ok(heads)
        })
    }

    /// Forgets one channel's saved head (`remove`).
    pub fn remove_head(&self, partition: &Partition, channel: &str) -> Result<(), StoreError> {
        self.with(|c| {
            let tx = c.transaction()?;
            for table in ["head_events", "head_meta"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE origin = ?1 AND viewer = ?2 AND channel_id = ?3"
                    ),
                    params![partition.origin, partition.viewer, channel],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Keeps only these channels' saved heads (`retain`).
    pub fn retain_heads(
        &self,
        partition: &Partition,
        channels: &std::collections::HashSet<String>,
    ) -> Result<(), StoreError> {
        let saved: Vec<String> = self.with(|c| {
            let mut query =
                c.prepare("SELECT channel_id FROM head_meta WHERE origin = ?1 AND viewer = ?2")?;
            Ok(query
                .query_map(params![partition.origin, partition.viewer], |row| {
                    row.get(0)
                })?
                .collect::<Result<_, _>>()?)
        })?;
        for channel in saved.iter().filter(|id| !channels.contains(*id)) {
            self.remove_head(partition, channel)?;
        }
        Ok(())
    }

    /// Wipes this partition's saved heads and roster snapshot (`clear`).
    pub fn clear_cache(&self, partition: &Partition) -> Result<(), StoreError> {
        self.with(|c| {
            let tx = c.transaction()?;
            for table in ["head_events", "head_meta"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE origin = ?1 AND viewer = ?2"),
                    params![partition.origin, partition.viewer],
                )?;
            }
            tx.execute(
                "UPDATE startup_cache SET discovery_json = NULL WHERE origin = ?1 AND viewer = ?2",
                params![partition.origin, partition.viewer],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The saved roster snapshot (`readStartup().discovery`), as saved.
    pub fn startup_discovery(&self, partition: &Partition) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT discovery_json FROM startup_cache WHERE origin = ?1 AND viewer = ?2",
                params![partition.origin, partition.viewer],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
    }

    /// Saves the roster snapshot (`writeStartup`). A record over the byte
    /// budget is not saved; then the oldest records across partitions go
    /// until the rest fit.
    pub fn save_startup_discovery(
        &self,
        partition: &Partition,
        json: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            let tx = c.transaction()?;
            let preferences: Option<String> = tx
                .query_row(
                    "SELECT preferences_json FROM startup_cache WHERE origin = ?1 AND viewer = ?2",
                    params![partition.origin, partition.viewer],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let bytes = json.len() + preferences.as_ref().map_or(0, String::len);
            if bytes > MAX_SAVED_BYTES {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO startup_cache (origin, viewer, discovery_json, preferences_json, saved_ms, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (origin, viewer) DO UPDATE SET
                   discovery_json = excluded.discovery_json, saved_ms = excluded.saved_ms,
                   bytes = excluded.bytes",
                params![partition.origin, partition.viewer, json, preferences, now_ms, bytes as i64],
            )?;
            let rows: Vec<(String, String, i64)> = {
                let mut query = tx.prepare(
                    "SELECT origin, viewer, bytes FROM startup_cache ORDER BY saved_ms DESC",
                )?;
                query
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<_, _>>()?
            };
            let mut total = 0usize;
            for (origin, viewer, bytes) in rows {
                total += bytes as usize;
                if total > MAX_SAVED_BYTES {
                    tx.execute(
                        "DELETE FROM startup_cache WHERE origin = ?1 AND viewer = ?2",
                        params![origin, viewer],
                    )?;
                }
            }
            tx.commit()?;
            Ok(true)
        })
    }

    /// Removes everything stored for one community, in one transaction.
    /// The session must be disposed first so nothing writes it again.
    pub fn purge(&self, partition: &Partition) -> Result<(), StoreError> {
        self.with(|c| {
            let tx = c.transaction()?;
            for table in PARTITION_TABLES {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE origin = ?1 AND viewer = ?2"),
                    params![partition.origin, partition.viewer],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Saved channel heads kept across every community (`persistence.ts`).
pub const MAX_SAVED_HEADS: usize = 64;
/// Bytes of saved heads, and of roster snapshots, across every community.
pub const MAX_SAVED_BYTES: usize = 8 << 20;
/// Saved heads and roster snapshots older than this are not used.
pub const MAX_SAVED_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// One saved head as loaded: unverified wire JSON. Never authority for
/// membership or freshness; the session re-verifies every event.
#[derive(Clone, Debug)]
pub struct SavedHead {
    pub channel: String,
    /// When the head was read from the relay, in Unix ms.
    pub saved_ms: i64,
    pub events: Vec<serde_json::Value>,
    /// The authors' kind-0 profiles.
    pub profiles: Vec<serde_json::Value>,
}

/// Newest first, drops heads past the global count, byte and age budgets.
fn trim_heads(tx: &rusqlite::Transaction<'_>, now_ms: i64) -> Result<(), StoreError> {
    let heads: Vec<(String, String, String, i64, i64)> = {
        let mut query = tx.prepare(
            "SELECT origin, viewer, channel_id, bytes, saved_ms FROM head_meta ORDER BY saved_ms DESC",
        )?;
        query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?
    };
    let mut total = 0usize;
    for (count, (origin, viewer, channel, bytes, saved_ms)) in heads.into_iter().enumerate() {
        total += bytes as usize;
        if total > MAX_SAVED_BYTES
            || count >= MAX_SAVED_HEADS
            || now_ms - saved_ms >= MAX_SAVED_AGE_MS
        {
            for table in ["head_events", "head_meta"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE origin = ?1 AND viewer = ?2 AND channel_id = ?3"
                    ),
                    params![origin, viewer, channel],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag};

    fn partition(origin: &str) -> Partition {
        Partition {
            origin: origin.into(),
            viewer: "ab".repeat(32),
        }
    }

    #[test]
    fn opens_files_and_refuses_newer_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/buzz.sqlite3");
        Store::open(&path).unwrap();
        // Reopening an existing database keeps its data.
        let store = Store::open(&path).unwrap();
        store.set_device_pref("theme", "\"dark\"").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.device_pref("theme").unwrap().as_deref(),
            Some("\"dark\"")
        );
        store
            .with(|c| Ok(c.pragma_update(None, "user_version", 99)?))
            .unwrap();
        drop(store);
        assert!(matches!(Store::open(&path), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn view_state_is_partitioned_and_removable() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let b = partition("https://b.example");
        store
            .set_view_state(&a, "draft:general", Some("\"hi\""))
            .unwrap();
        assert_eq!(
            store.view_state(&a, "draft:general").unwrap().as_deref(),
            Some("\"hi\"")
        );
        assert_eq!(store.view_state(&b, "draft:general").unwrap(), None);
        store.set_view_state(&a, "draft:general", None).unwrap();
        assert_eq!(store.view_state(&a, "draft:general").unwrap(), None);
    }

    #[test]
    fn view_states_list_one_prefix_in_one_partition() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let b = partition("https://b.example");
        store.set_view_state(&a, "draft:one", Some("1")).unwrap();
        store
            .set_view_state(&a, "selected-channel", Some("2"))
            .unwrap();
        store.set_view_state(&b, "draft:two", Some("3")).unwrap();
        let drafts = store.view_states(&a, "draft:").unwrap();
        assert_eq!(drafts, vec![("draft:one".to_owned(), "1".to_owned())]);
    }

    fn signed(keys: &Keys, content: &str) -> crate::Verified {
        let event = EventBuilder::new(Kind::from_u16(9), content)
            .tags([Tag::custom("h", ["general"])])
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn heads_round_trip_and_purge_clears_only_one_partition() {
        let store = Store::in_memory().unwrap();
        let verifier = Verifier::default();
        let keys = Keys::generate();
        let events: Vec<_> = (0..3).map(|i| signed(&keys, &format!("m{i}"))).collect();
        let a = partition("https://a.example");
        let b = partition("https://b.example");
        let now = now_ms();
        assert!(
            store
                .save_head(&a, "general", now, &events, &events[..1], now)
                .unwrap()
        );
        store
            .save_head(&b, "general", now, &events[..1], &[], now)
            .unwrap();
        let heads = store.load_heads(&a, now).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].saved_ms, now);
        assert_eq!(heads[0].events.len(), 3);
        assert_eq!(heads[0].profiles.len(), 1);
        assert!(verifier.verify_json(&heads[0].events[0]).is_ok());

        store.purge(&a).unwrap();
        assert!(store.load_heads(&a, now).unwrap().is_empty());
        assert_eq!(store.load_heads(&b, now).unwrap().len(), 1);
    }

    #[test]
    fn head_budgets_drop_the_oldest_and_the_stale() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let now = now_ms();
        for n in 0..=MAX_SAVED_HEADS as i64 {
            store
                .save_head(&a, &format!("c{n}"), now - 1_000 + n, &[], &[], now)
                .unwrap();
        }
        let heads = store.load_heads(&a, now).unwrap();
        assert_eq!(heads.len(), MAX_SAVED_HEADS);
        assert!(
            heads.iter().all(|head| head.channel != "c0"),
            "the oldest went"
        );
        // Older than a day: neither loaded nor kept.
        store
            .save_head(&a, "old", now - MAX_SAVED_AGE_MS, &[], &[], now)
            .unwrap();
        assert!(
            !store
                .load_heads(&a, now)
                .unwrap()
                .iter()
                .any(|h| h.channel == "old")
        );

        store.remove_head(&a, "c1").unwrap();
        let keep: std::collections::HashSet<String> = ["c2".to_string()].into();
        store.retain_heads(&a, &keep).unwrap();
        let heads = store.load_heads(&a, now).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].channel, "c2");

        // Over the byte budget: not saved at all.
        let keys = Keys::generate();
        let big = signed(&keys, &"x".repeat(MAX_SAVED_BYTES));
        assert!(!store.save_head(&a, "big", now, &[big], &[], now).unwrap());
    }

    #[test]
    fn startup_snapshots_are_per_partition_and_cleared_with_heads() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let b = partition("https://b.example");
        let now = now_ms();
        assert!(store.save_startup_discovery(&a, "{\"a\":1}", now).unwrap());
        store.save_startup_discovery(&b, "{\"b\":1}", now).unwrap();
        store.save_head(&a, "general", now, &[], &[], now).unwrap();
        assert_eq!(
            store.startup_discovery(&a).unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        store.clear_cache(&a).unwrap();
        assert_eq!(store.startup_discovery(&a).unwrap(), None);
        assert!(store.load_heads(&a, now).unwrap().is_empty());
        assert_eq!(
            store.startup_discovery(&b).unwrap().as_deref(),
            Some("{\"b\":1}")
        );
        assert!(
            !store
                .save_startup_discovery(&a, &"x".repeat(MAX_SAVED_BYTES + 1), now)
                .unwrap()
        );
    }
}

/// The outbox journal for one partition (spec §6.5, §7.5). Every commit
/// makes the partition's rows match the whole journal in one transaction,
/// writing only the rows that changed, so a crash leaves either the old
/// journal or the new one.
#[derive(Clone)]
pub struct SqliteJournal {
    store: Store,
    partition: Partition,
}

impl SqliteJournal {
    pub fn new(store: Store, partition: Partition) -> Self {
        Self { store, partition }
    }
}

impl crate::outbox::Journal for SqliteJournal {
    fn load(
        &self,
    ) -> futures::future::BoxFuture<'static, Result<Vec<crate::outbox::Operation>, String>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.load_blocking())
                .await
                .map_err(|_| "Outbox load was interrupted".to_string())?
                .map_err(|error| error.to_string())
        })
    }

    fn commit(
        &self,
        operations: Vec<crate::outbox::Operation>,
    ) -> futures::future::BoxFuture<'static, Result<(), String>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.commit_blocking(&operations))
                .await
                .map_err(|_| "Outbox commit was interrupted".to_string())?
                .map_err(|error| error.to_string())
        })
    }
}

impl SqliteJournal {
    fn load_blocking(&self) -> Result<Vec<crate::outbox::Operation>, StoreError> {
        use crate::outbox::{Delivery, Operation, Recovery};
        self.store.with(|c| {
            let mut query = c.prepare(
                "SELECT template_json, signed_json, delivery, error, recovery_key, recovery_value,
                        acknowledged, guarded
                 FROM outbox_ops WHERE origin = ?1 AND viewer = ?2 ORDER BY enqueued_seq",
            )?;
            let rows = query
                .query_map(
                    params![self.partition.origin, self.partition.viewer],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, bool>(6)?,
                            row.get::<_, bool>(7)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(
                    |(template, signed, delivery, error, key, value, acknowledged, guarded)| {
                        let corrupt = |what: &str| StoreError::Corrupt(format!("outbox {what}"));
                        let delivery: Delivery =
                            serde_json::from_value(serde_json::Value::String(delivery))
                                .map_err(|_| corrupt("delivery"))?;
                        Ok(Operation {
                            event: serde_json::from_str(&template)
                                .map_err(|_| corrupt("template"))?,
                            signed: signed
                                .map(|json| serde_json::from_str(&json))
                                .transpose()
                                .map_err(|_| corrupt("signed event"))?,
                            recovery: match (key, value) {
                                (Some(key), Some(value)) => Some(Recovery { key, value }),
                                (None, None) => None,
                                _ => return Err(corrupt("recovery")),
                            },
                            acknowledged,
                            guarded,
                            delivery,
                            error,
                        })
                    },
                )
                .collect()
        })
    }

    fn commit_blocking(&self, operations: &[crate::outbox::Operation]) -> Result<(), StoreError> {
        use std::collections::{HashMap, HashSet};
        /// The parts of a row that change after its first write; the
        /// template is fixed by the event id.
        #[derive(PartialEq)]
        struct Saved {
            seq: i64,
            signed: bool,
            delivery: String,
            error: Option<String>,
            recovery_key: Option<String>,
            recovery_value: Option<String>,
            acknowledged: bool,
            guarded: bool,
            confirmed: bool,
        }
        self.store.with(|c| {
            let tx = c.transaction()?;
            let saved: HashMap<String, Saved> = {
                let mut query = tx.prepare(
                    "SELECT event_id, enqueued_seq, signed_json IS NOT NULL, delivery, error,
                       recovery_key, recovery_value, acknowledged, guarded, confirmed
                     FROM outbox_ops WHERE origin = ?1 AND viewer = ?2",
                )?;
                query
                    .query_map(
                        params![self.partition.origin, self.partition.viewer],
                        |row| {
                            Ok((
                                row.get(0)?,
                                Saved {
                                    seq: row.get(1)?,
                                    signed: row.get(2)?,
                                    delivery: row.get(3)?,
                                    error: row.get(4)?,
                                    recovery_key: row.get(5)?,
                                    recovery_value: row.get(6)?,
                                    acknowledged: row.get(7)?,
                                    guarded: row.get(8)?,
                                    confirmed: row.get(9)?,
                                },
                            ))
                        },
                    )?
                    .collect::<Result<_, _>>()?
            };
            let ids: HashSet<String> = operations.iter().map(|op| op.id().to_hex()).collect();
            {
                let mut delete = tx.prepare(
                    "DELETE FROM outbox_ops WHERE origin = ?1 AND viewer = ?2 AND event_id = ?3",
                )?;
                for id in saved.keys().filter(|id| !ids.contains(*id)) {
                    delete.execute(params![self.partition.origin, self.partition.viewer, id])?;
                }
            }
            // Only rows that changed are written. A row keeps its sequence
            // while the journal order allows, so removals and appends do
            // not renumber the rest.
            let mut changed = Vec::new();
            let mut last: Option<i64> = None;
            for op in operations {
                let id = op.id().to_hex();
                let previous = saved.get(&id);
                let seq = match previous.map(|row| row.seq) {
                    Some(seq) if last.is_none_or(|last| seq > last) => seq,
                    _ => last.map_or(0, |last| last + 1),
                };
                last = Some(seq);
                let row = Saved {
                    seq,
                    signed: op.signed.is_some(),
                    delivery: serde_json::to_value(op.delivery)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    error: op.error.clone(),
                    recovery_key: op.recovery.as_ref().map(|r| r.key.clone()),
                    recovery_value: op.recovery.as_ref().map(|r| r.value.clone()),
                    acknowledged: op.acknowledged,
                    guarded: op.guarded,
                    confirmed: matches!(op.delivery, crate::outbox::Delivery::Seen)
                        && op.recovery.is_none(),
                };
                if previous != Some(&row) {
                    changed.push((op, id, row));
                }
            }
            // Rows giving up a recovery key go first, so the unique index
            // never sees the key twice.
            changed.sort_by_key(|(_, _, row)| row.recovery_key.is_some() && !row.confirmed);
            {
                let mut upsert = tx.prepare(
                    "INSERT INTO outbox_ops (origin, viewer, event_id, kind, created_at, channel_id,
                       template_json, signed_json, delivery, error, recovery_key, recovery_value,
                       acknowledged, guarded, confirmed, enqueued_seq, bytes, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                     ON CONFLICT (origin, viewer, event_id) DO UPDATE SET
                       kind = excluded.kind, created_at = excluded.created_at,
                       channel_id = excluded.channel_id, template_json = excluded.template_json,
                       signed_json = excluded.signed_json, delivery = excluded.delivery,
                       error = excluded.error, recovery_key = excluded.recovery_key,
                       recovery_value = excluded.recovery_value,
                       acknowledged = excluded.acknowledged, guarded = excluded.guarded,
                       confirmed = excluded.confirmed, enqueued_seq = excluded.enqueued_seq,
                       bytes = excluded.bytes, updated_ms = excluded.updated_ms",
                )?;
                let now = now_ms();
                for (op, id, row) in changed {
                    let template = serde_json::to_string(&op.event)
                        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
                    let signed = op.signed.as_ref().map(|event| event.as_json());
                    let bytes = template.len() + signed.as_ref().map_or(0, String::len);
                    upsert.execute(params![
                        self.partition.origin,
                        self.partition.viewer,
                        id,
                        i64::from(op.kind()),
                        op.created_at() as i64,
                        op.channel(),
                        template,
                        signed,
                        row.delivery,
                        row.error,
                        row.recovery_key,
                        row.recovery_value,
                        row.acknowledged,
                        row.guarded,
                        row.confirmed,
                        row.seq,
                        bytes as i64,
                        now,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    use crate::outbox::{Delivery, Journal, Operation, Recovery};
    use buzz_identity::{Identity, Purpose};

    fn operation(identity: &Identity, content: &str, delivery: Delivery, sign: bool) -> Operation {
        let unsigned = identity
            .template(
                Purpose::Content,
                9,
                1_700_000_000,
                vec![vec!["h".into(), "general".into()]],
                content.into(),
            )
            .unwrap();
        Operation {
            signed: sign.then(|| identity.sign(Purpose::Content, unsigned.clone()).unwrap()),
            event: unsigned,
            recovery: None,
            acknowledged: false,
            guarded: false,
            delivery,
            error: None,
        }
    }

    #[tokio::test]
    async fn commits_replace_the_whole_journal_in_order() {
        let store = Store::in_memory().unwrap();
        let partition = Partition {
            origin: "https://a.example".into(),
            viewer: "ab".repeat(32),
        };
        let journal = SqliteJournal::new(store.clone(), partition.clone());
        let identity = Identity::ephemeral();
        let mut first = operation(&identity, "one", Delivery::Unknown, true);
        first.recovery = Some(Recovery {
            key: "draft:general".into(),
            value: "{}".into(),
        });
        let second = operation(&identity, "two", Delivery::Sending, false);
        journal
            .commit(vec![first.clone(), second.clone()])
            .await
            .unwrap();

        let loaded = journal.load().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id(), first.id());
        assert_eq!(
            loaded[0].signed.as_ref().unwrap().sig,
            first.signed.as_ref().unwrap().sig
        );
        assert_eq!(loaded[0].recovery, first.recovery);
        assert_eq!(loaded[1].delivery, Delivery::Sending);
        assert!(loaded[1].signed.is_none());

        journal.commit(vec![second]).await.unwrap();
        assert_eq!(journal.load().await.unwrap().len(), 1);

        // Another partition is untouched by this one's commits.
        let other = SqliteJournal::new(
            store,
            Partition {
                origin: "https://b.example".into(),
                ..partition
            },
        );
        assert!(other.load().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn commits_write_only_changed_rows_and_keep_order() {
        let store = Store::in_memory().unwrap();
        let partition = Partition {
            origin: "https://a.example".into(),
            viewer: "ab".repeat(32),
        };
        let journal = SqliteJournal::new(store.clone(), partition);
        let identity = Identity::ephemeral();
        let key = Recovery {
            key: "draft:general".into(),
            value: "{}".into(),
        };
        let mut one = operation(&identity, "one", Delivery::Accepted, true);
        one.recovery = Some(key.clone());
        let two = operation(&identity, "two", Delivery::Sending, false);
        let three = operation(&identity, "three", Delivery::Sending, false);
        journal
            .commit(vec![one.clone(), two.clone(), three.clone()])
            .await
            .unwrap();
        store
            .with(|c| Ok(c.execute("UPDATE outbox_ops SET updated_ms = 0", [])?))
            .unwrap();

        // The first leaves and hands its recovery key to a new last one.
        let mut four = operation(&identity, "four", Delivery::Sending, false);
        four.recovery = Some(key);
        let mut sent = three.clone();
        sent.delivery = Delivery::Accepted;
        journal
            .commit(vec![two.clone(), sent, four.clone()])
            .await
            .unwrap();
        let ids: Vec<_> = journal
            .load()
            .await
            .unwrap()
            .iter()
            .map(|op| op.id())
            .collect();
        assert_eq!(ids, [two.id(), three.id(), four.id()]);
        let untouched: Vec<String> = store
            .with(|c| {
                let mut query =
                    c.prepare("SELECT event_id FROM outbox_ops WHERE updated_ms = 0")?;
                Ok(query
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(untouched, [two.id().to_hex()]);
    }
}

/// The NIP-RS read-state journal for one partition (spec §5i.4): one JSON
/// record in `read_state`. The record is returned exactly as saved;
/// [`crate::read_state::ReadStateSync`] validates it, so a corrupt record
/// is an error and is never replaced by an empty journal (§9.2 #25).
#[derive(Clone)]
pub struct SqliteReadJournal {
    store: Store,
    partition: Partition,
}

impl SqliteReadJournal {
    pub fn new(store: Store, partition: Partition) -> Self {
        Self { store, partition }
    }
}

fn read_state_error(error: StoreError) -> crate::read_state::ReadStateError {
    crate::read_state::ReadStateError::Storage(error.to_string())
}

impl crate::read_state::ReadStateJournal for SqliteReadJournal {
    fn load(&mut self) -> Result<Option<String>, crate::read_state::ReadStateError> {
        self.store
            .with(|c| {
                Ok(c.query_row(
                    "SELECT journal_json FROM read_state WHERE origin = ?1 AND viewer = ?2",
                    params![self.partition.origin, self.partition.viewer],
                    |row| row.get(0),
                )
                .optional()?)
            })
            .map_err(read_state_error)
    }

    fn store(&mut self, record: &str) -> Result<(), crate::read_state::ReadStateError> {
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO read_state (origin, viewer, journal_json) VALUES (?1, ?2, ?3)
                     ON CONFLICT (origin, viewer) DO UPDATE SET journal_json = excluded.journal_json",
                    params![self.partition.origin, self.partition.viewer, record],
                )?;
                Ok(())
            })
            .map_err(read_state_error)
    }

    fn purge(&mut self) -> Result<(), crate::read_state::ReadStateError> {
        self.store
            .with(|c| {
                c.execute(
                    "DELETE FROM read_state WHERE origin = ?1 AND viewer = ?2",
                    params![self.partition.origin, self.partition.viewer],
                )?;
                Ok(())
            })
            .map_err(read_state_error)
    }
}

/// Device-local thread follows for one partition (spec §5i.9), saved as
/// the reference's JSON array in `partition_prefs`. As with the
/// reference's local storage, unreadable data reads as no choices.
#[derive(Clone)]
pub struct SqliteThreadFollows {
    store: Store,
    partition: Partition,
}

impl SqliteThreadFollows {
    pub const NAME: &'static str = "thread-follows";

    pub fn new(store: Store, partition: Partition) -> Self {
        Self { store, partition }
    }
}

impl crate::unread::ThreadFollowStore for SqliteThreadFollows {
    fn read(&mut self) -> Vec<(String, bool)> {
        self.store
            .partition_pref(&self.partition, Self::NAME)
            .ok()
            .flatten()
            .map(|text| crate::unread::parse_thread_follows(&text))
            .unwrap_or_default()
    }

    fn write(&mut self, choices: &[(String, bool)]) -> Result<(), String> {
        self.store
            .set_partition_pref(
                &self.partition,
                Self::NAME,
                &crate::unread::encode_thread_follows(choices),
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod read_state_tests {
    use super::*;
    use crate::read_state::{
        ReadCapability, ReadStateJournal, ReadStateSync, ReadSyncStatus, ReadUpdate,
    };
    use crate::unread::ThreadFollowStore;

    fn partition(origin: &str) -> Partition {
        Partition {
            origin: origin.into(),
            viewer: "ab".repeat(32),
        }
    }

    #[test]
    fn read_journals_persist_per_partition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buzz.sqlite3");
        let a = partition("https://a.example");
        let viewer = a.viewer.clone();
        let store = Store::open(&path).unwrap();
        let mut sync = ReadStateSync::open(
            &viewer,
            ReadCapability::FrontierSync,
            SqliteReadJournal::new(store.clone(), a.clone()),
        );
        sync.apply(ReadUpdate::observe("general", 42), None, 1_000)
            .unwrap();
        let slot = sync.journal().unwrap().slot.clone();
        drop((sync, store));

        let store = Store::open(&path).unwrap();
        let sync = ReadStateSync::open(
            &viewer,
            ReadCapability::FrontierSync,
            SqliteReadJournal::new(store.clone(), a.clone()),
        );
        let journal = sync.journal().unwrap();
        assert_eq!(journal.slot, slot);
        assert_eq!(journal.state.frontier("general"), Some(42));

        // Another community starts its own journal.
        let mut other = SqliteReadJournal::new(store.clone(), partition("https://b.example"));
        assert_eq!(other.load().unwrap(), None);

        store.purge(&a).unwrap();
        assert_eq!(
            SqliteReadJournal::new(store, a).load().unwrap(),
            None,
            "leaving the community deletes its journal"
        );
    }

    #[test]
    fn a_corrupt_read_journal_is_an_error_and_kept() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let mut journal = SqliteReadJournal::new(store.clone(), a.clone());
        journal.store("{\"version\":1}").unwrap();
        let mut sync = ReadStateSync::open(&a.viewer, ReadCapability::FrontierSync, journal);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Error);
        assert!(sync.journal().is_none());
        assert!(
            sync.apply(ReadUpdate::observe("general", 1), None, 1_000)
                .is_err()
        );
        assert_eq!(
            sync.into_store().load().unwrap().as_deref(),
            Some("{\"version\":1}")
        );
        let mut purge = SqliteReadJournal::new(store, a);
        purge.purge().unwrap();
        assert_eq!(purge.load().unwrap(), None);
    }

    #[test]
    fn thread_follows_round_trip_per_partition() {
        let store = Store::in_memory().unwrap();
        let a = partition("https://a.example");
        let mut follows = SqliteThreadFollows::new(store.clone(), a.clone());
        assert!(follows.read().is_empty());
        let choices = vec![
            (format!("general:{}", "1".repeat(64)), true),
            (format!("general:{}", "2".repeat(64)), false),
        ];
        follows.write(&choices).unwrap();
        assert_eq!(
            SqliteThreadFollows::new(store.clone(), a.clone()).read(),
            choices
        );
        assert!(
            SqliteThreadFollows::new(store.clone(), partition("https://b.example"))
                .read()
                .is_empty()
        );

        // Unreadable data is no choices, like the reference's storage.
        store
            .set_partition_pref(&a, SqliteThreadFollows::NAME, "not json")
            .unwrap();
        assert!(follows.read().is_empty());

        follows.write(&choices[..1]).unwrap();
        store.purge(&a).unwrap();
        assert!(follows.read().is_empty());
    }
}
