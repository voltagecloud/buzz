//! Saved agent activity (buzz-app's `src-tauri/src/archive`): a bounded
//! SQLite archive of the encrypted agent activity frames (kind 24200) and
//! turn metrics (kind 44200) agents send their owner.
//!
//! Only the signed, still-encrypted envelopes are stored, partitioned by
//! viewer and community. Activity keeps 1 to 90 days (30 by default) within
//! 512 MiB per community and a quarter of that per agent; metrics keep 90
//! days within 64 MiB. Capture runs while a community is connected, even
//! when nothing shows the activity.

use crate::event::kinds;
use buzz_identity::Identity;
use nostr::prelude::Event;
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The schema buzz-app's archive uses (`schema.sql`, version 2).
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS archive_partitions (
  viewer TEXT NOT NULL, community TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (viewer, community)
);
CREATE TABLE IF NOT EXISTS archive_subscriptions (
  viewer TEXT NOT NULL, community TEXT NOT NULL, name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  days INTEGER NOT NULL, budget INTEGER NOT NULL,
  PRIMARY KEY (viewer, community, name)
);
CREATE TABLE IF NOT EXISTS archive_events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  viewer TEXT NOT NULL, community TEXT NOT NULL, subscription TEXT NOT NULL,
  id TEXT NOT NULL, agent TEXT NOT NULL, kind INTEGER NOT NULL,
  created INTEGER NOT NULL, received INTEGER NOT NULL, bytes INTEGER NOT NULL,
  envelope TEXT NOT NULL,
  UNIQUE (viewer, community, id)
);
CREATE INDEX IF NOT EXISTS archive_page ON archive_events(viewer, community, kind, seq DESC);
CREATE INDEX IF NOT EXISTS archive_agent_page ON archive_events(viewer, community, kind, agent, seq DESC);
CREATE INDEX IF NOT EXISTS archive_expiry ON archive_events(viewer, community, subscription, created);
CREATE TABLE IF NOT EXISTS archive_usage (
  viewer TEXT NOT NULL, community TEXT NOT NULL, subscription TEXT NOT NULL,
  agent TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(viewer,community,subscription,agent)
);
CREATE TRIGGER IF NOT EXISTS archive_insert_usage AFTER INSERT ON archive_events BEGIN
  INSERT INTO archive_usage VALUES(NEW.viewer,NEW.community,NEW.subscription,NEW.agent,NEW.bytes)
    ON CONFLICT(viewer,community,subscription,agent) DO UPDATE SET bytes=bytes+NEW.bytes;
  INSERT INTO archive_usage VALUES(NEW.viewer,NEW.community,NEW.subscription,'',NEW.bytes)
    ON CONFLICT(viewer,community,subscription,agent) DO UPDATE SET bytes=bytes+NEW.bytes;
END;
CREATE TRIGGER IF NOT EXISTS archive_delete_usage AFTER DELETE ON archive_events BEGIN
  UPDATE archive_usage SET bytes=bytes-OLD.bytes WHERE viewer=OLD.viewer AND community=OLD.community
    AND subscription=OLD.subscription AND agent IN ('',OLD.agent);
END;
"#;

/// Retention choices the settings offer, in days.
pub const RETENTION_DAYS: [u32; 4] = [1, 7, 30, 90];
/// Envelopes older than this when they arrive are not archived.
const FRESHNESS_SECS: u64 = 300;
/// Saved envelopes older than this are not read back (`decode_archive`
/// with `history`).
const HISTORY_SECS: u64 = 90 * 24 * 60 * 60;
/// Records per [`Archive::read`] page.
pub const PAGE_SIZE: usize = 100;
const MAX_CONTENT: usize = 87_472;
const MIN_CONTENT: usize = 132;
const MAX_FRAME: usize = 65_535;

type Result<T> = std::result::Result<T, String>;

fn database<T>(result: rusqlite::Result<T>) -> Result<T> {
    result.map_err(|_| "Local archive storage failed; retry without deleting the archive".into())
}

/// One account's archive settings in one community.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSettings {
    /// "Save agent activity".
    pub observer: bool,
    /// "Save agent turn metrics".
    pub metrics: bool,
    /// "Activity retention", 1 to 90 days.
    pub observer_days: u32,
    /// Changes with every configure and clear; a capture started under an
    /// older revision stops archiving.
    pub revision: i64,
    /// Encrypted bytes saved for this account and community.
    pub bytes: i64,
}

impl ArchiveSettings {
    /// The kinds to capture.
    pub fn kinds(&self) -> Vec<u16> {
        let mut kinds = Vec::new();
        if self.observer {
            kinds.push(kinds::AGENT_ACTIVITY);
        }
        if self.metrics {
            kinds.push(kinds::AGENT_METRICS);
        }
        kinds
    }
}

/// One saved record, verified and decrypted for the viewer (an
/// `ArchivePage` record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveRecord {
    /// The envelope's event id.
    pub id: String,
    /// The agent that signed it.
    pub agent: String,
    pub created_at: u64,
    /// When it was saved, in seconds.
    pub received_at: i64,
    /// The decrypted frame: JSON.
    pub plaintext: String,
}

/// A page of saved records, newest first (`ArchivePage`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchivePage {
    pub records: Vec<ArchiveRecord>,
    /// Saved records that no longer verify or decrypt.
    pub skipped: usize,
    /// The cursor for the next, older page; `None` on the last page.
    pub before: Option<i64>,
    /// The settings revision the page was read under.
    pub revision: i64,
}

pub struct Archive {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    connection: Connection,
    /// When expired records were last removed.
    pruned: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

impl Archive {
    pub fn open(path: PathBuf) -> Result<Self> {
        let parent = path.parent().ok_or("Archive location unavailable")?;
        std::fs::create_dir_all(parent).map_err(|_| "Archive directory unavailable")?;
        let connection = database(Connection::open(&path))?;
        database(connection.busy_timeout(std::time::Duration::from_secs(2)))?;
        // Auto-vacuum must be chosen before the first table exists.
        let vacuum: u32 = database(connection.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)))?;
        if vacuum == 0 {
            database(connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL"))?;
        }
        database(connection.execute_batch("BEGIN IMMEDIATE"))?;
        let version: u32 = database(connection.query_row("PRAGMA user_version", [], |r| r.get(0)))?;
        if version > 2 {
            database(connection.execute_batch("ROLLBACK"))?;
            return Err("Archive was created by a newer app".into());
        }
        database(connection.execute_batch(SCHEMA))?;
        database(connection.execute_batch(
            "PRAGMA user_version=2; COMMIT; PRAGMA journal_mode=WAL; PRAGMA secure_delete=ON;",
        ))?;
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                connection,
                pruned: 0,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| "Archive unavailable".into())
    }

    /// The settings, creating the defaults on first use.
    pub fn settings(&self, viewer: &str, community: &str) -> Result<ArchiveSettings> {
        let mut inner = self.lock()?;
        inner.seed(viewer, community)?;
        inner.prune(now())?;
        inner.settings(viewer, community)
    }

    /// Saves the choices, if nothing changed them since `revision`.
    /// Shorter retention deletes what it no longer keeps.
    pub fn configure(
        &self,
        viewer: &str,
        community: &str,
        observer: bool,
        metrics: bool,
        observer_days: u32,
        revision: i64,
    ) -> Result<ArchiveSettings> {
        if !(1..=90).contains(&observer_days) {
            return Err("Observer retention must be 1 to 90 days".into());
        }
        let mut inner = self.lock()?;
        inner.seed(viewer, community)?;
        {
            let tx = database(
                inner
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate),
            )?;
            let changed = database(tx.execute(
                "UPDATE archive_partitions SET revision=revision+1 WHERE viewer=?1 AND community=?2 AND revision=?3",
                params![viewer, community, revision],
            ))?;
            if changed != 1 {
                return Err("Archive settings changed; reload and retry".into());
            }
            database(tx.execute(
                "UPDATE archive_subscriptions SET enabled=?3, days=?4 WHERE viewer=?1 AND community=?2 AND name='observer'",
                params![viewer, community, observer, observer_days],
            ))?;
            database(tx.execute(
                "UPDATE archive_subscriptions SET enabled=?3 WHERE viewer=?1 AND community=?2 AND name='metrics'",
                params![viewer, community, metrics],
            ))?;
            database(tx.commit())?;
        }
        inner.pruned = 0;
        inner.prune(now())?;
        inner.settings(viewer, community)
    }

    /// Deletes this account's records of `kind` (or both) in the community.
    pub fn clear(&self, viewer: &str, community: &str, kind: Option<u16>) -> Result<()> {
        if kind.is_some_and(|kind| !matches!(kind, kinds::AGENT_ACTIVITY | kinds::AGENT_METRICS)) {
            return Err("Unsupported archive kind".into());
        }
        let mut inner = self.lock()?;
        {
            let tx = database(
                inner
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate),
            )?;
            database(tx.execute(
                "UPDATE archive_partitions SET revision=revision+1 WHERE viewer=?1 AND community=?2",
                params![viewer, community],
            ))?;
            database(tx.execute(
                "DELETE FROM archive_events WHERE viewer=?1 AND community=?2 AND (?3 IS NULL OR kind=?3)",
                params![viewer, community, kind],
            ))?;
            database(tx.commit())?;
        }
        // The delete committed; reclaiming space is best effort.
        let _ = inner.vacuum(0);
        let _ = inner
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        Ok(())
    }

    /// The agents with saved records of `kind` for the viewer, at most
    /// 256 (`ArchivePage.agents`).
    pub fn agents(&self, viewer: &str, community: &str, kind: u16) -> Result<Vec<String>> {
        let inner = self.lock()?;
        let mut statement = database(inner.connection.prepare(
            "SELECT DISTINCT agent FROM archive_events WHERE viewer=?1 AND community=?2 AND kind=?3 ORDER BY agent LIMIT 256",
        ))?;
        let rows = database(
            statement.query_map(params![viewer, community, kind], |r| r.get::<_, String>(0)),
        )?;
        database(rows.collect::<rusqlite::Result<Vec<_>>>())
    }

    /// One page of the viewer's saved records of `kind`, newest first,
    /// optionally from one `agent` and older than the `before` cursor
    /// (buzz-app's archive `read`). Records past their retention are left
    /// out. Each record is verified and decrypted again; those that fail
    /// count as skipped.
    pub fn read(
        &self,
        identity: &Identity,
        community: &str,
        kind: u16,
        agent: Option<&str>,
        before: Option<i64>,
    ) -> Result<ArchivePage> {
        if !matches!(kind, kinds::AGENT_ACTIVITY | kinds::AGENT_METRICS) {
            return Err("Unsupported archive kind".into());
        }
        if agent.is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
            || before.is_some_and(|v| v <= 0)
        {
            return Err("Invalid archive page".into());
        }
        let viewer = identity.pubkey_hex();
        let (rows, revision) = {
            let inner = self.lock()?;
            inner.seed(&viewer, community)?;
            let mut statement = database(inner.connection.prepare(
                "SELECT seq,received,envelope FROM archive_events WHERE viewer=?1 AND community=?2 AND kind=?3 AND (?4 IS NULL OR agent=?4) AND (?5 IS NULL OR seq<?5) AND created > ?6-(SELECT days*86400 FROM archive_subscriptions WHERE viewer=?1 AND community=?2 AND name=subscription) ORDER BY seq DESC LIMIT ?7",
            ))?;
            let rows = database(statement.query_map(
                params![
                    viewer,
                    community,
                    kind,
                    agent,
                    before,
                    now(),
                    PAGE_SIZE as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            ))?;
            let rows = database(rows.collect::<rusqlite::Result<Vec<_>>>())?;
            (rows, inner.settings(&viewer, community)?.revision)
        };
        let before = if rows.len() == PAGE_SIZE {
            rows.last().map(|row| row.0)
        } else {
            None
        };
        let now = now().max(0) as u64;
        let mut page = ArchivePage {
            before,
            revision,
            ..ArchivePage::default()
        };
        for (_, received_at, raw) in rows {
            let opened = serde_json::from_str::<Event>(&raw)
                .ok()
                .filter(|event| event.verify().is_ok())
                .and_then(|event| {
                    let plaintext = open_envelope(identity, &event, now, HISTORY_SECS).ok()?;
                    Some(ArchiveRecord {
                        id: event.id.to_hex(),
                        agent: event.pubkey.to_hex(),
                        created_at: event.created_at.as_secs(),
                        received_at,
                        plaintext,
                    })
                });
            match opened {
                Some(record) => page.records.push(record),
                None => page.skipped += 1,
            }
        }
        Ok(page)
    }

    /// Stores a checked envelope (see [`check_envelope`]) if its kind is
    /// captured and the settings are still at `revision`, then trims the
    /// oldest records over the byte budgets.
    pub fn ingest(
        &self,
        viewer: &str,
        community: &str,
        event: &Event,
        revision: i64,
    ) -> Result<()> {
        let kind = event.kind.as_u16();
        let name = match kind {
            kinds::AGENT_ACTIVITY => "observer",
            kinds::AGENT_METRICS => "metrics",
            _ => return Err("Unsupported archive kind".into()),
        };
        let raw = serde_json::to_string(event).map_err(|_| "Invalid archive envelope")?;
        let agent = event.pubkey.to_hex();
        let mut inner = self.lock()?;
        inner.seed(viewer, community)?;
        let tx = database(
            inner
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        let current: i64 = database(tx.query_row(
            "SELECT revision FROM archive_partitions WHERE viewer=?1 AND community=?2",
            params![viewer, community],
            |r| r.get(0),
        ))?;
        if current != revision {
            return Err("Archive revision changed".into());
        }
        let (enabled, budget): (bool, i64) = database(tx.query_row(
            "SELECT enabled,budget FROM archive_subscriptions WHERE viewer=?1 AND community=?2 AND name=?3",
            params![viewer, community, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ))?;
        if enabled {
            database(tx.execute(
                "INSERT OR IGNORE INTO archive_events(viewer,community,subscription,id,agent,kind,created,received,bytes,envelope) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    viewer,
                    community,
                    name,
                    event.id.to_hex(),
                    agent,
                    kind,
                    event.created_at.as_secs() as i64,
                    now(),
                    raw.len() as i64,
                    raw
                ],
            ))?;
            // Byte budgets, not frame counts; one agent cannot fill the pool.
            for (owner, cap) in [(Some(agent.as_str()), budget / 4), (None, budget)] {
                let used: i64 = database(tx.query_row(
                    "SELECT bytes FROM archive_usage WHERE viewer=?1 AND community=?2 AND subscription=?3 AND agent=?4",
                    params![viewer, community, name, owner.unwrap_or("")],
                    |r| r.get(0),
                ))?;
                if used <= cap {
                    continue;
                }
                let suffix = if owner.is_some() { " AND agent=?4" } else { "" };
                let cutoff = {
                    let mut statement = database(tx.prepare(&format!(
                        "SELECT seq,bytes FROM archive_events WHERE viewer=?1 AND community=?2 AND kind=?3{suffix} ORDER BY seq"
                    )))?;
                    let mut rows = database(match owner {
                        Some(owner) => statement.query(params![viewer, community, kind, owner]),
                        None => statement.query(params![viewer, community, kind]),
                    })?;
                    let mut overflow = used - cap;
                    let mut cutoff = 0;
                    while overflow > 0 {
                        let row = database(rows.next())?.ok_or("Archive usage mismatch")?;
                        cutoff = database(row.get::<_, i64>(0))?;
                        overflow -= database(row.get::<_, i64>(1))?;
                    }
                    cutoff
                };
                database(tx.execute(
                    &format!(
                        "DELETE FROM archive_events WHERE viewer=?1 AND community=?2 AND kind=?3{suffix} AND seq<=?5"
                    ),
                    params![viewer, community, kind, owner, cutoff],
                ))?;
            }
        }
        database(tx.commit())
    }
}

impl Inner {
    fn seed(&self, viewer: &str, community: &str) -> Result<()> {
        database(self.connection.execute(
            "INSERT OR IGNORE INTO archive_partitions(viewer,community) VALUES (?1,?2)",
            params![viewer, community],
        ))?;
        for (name, days, budget) in [
            ("observer", 30, 512 * 1024 * 1024_i64),
            ("metrics", 90, 64 * 1024 * 1024),
        ] {
            database(self.connection.execute(
                "INSERT OR IGNORE INTO archive_subscriptions(viewer,community,name,days,budget) VALUES (?1,?2,?3,?4,?5)",
                params![viewer, community, name, days, budget],
            ))?;
        }
        Ok(())
    }

    fn settings(&self, viewer: &str, community: &str) -> Result<ArchiveSettings> {
        // One statement, one snapshot: values and revision cannot tear.
        database(self.connection.query_row(
            "SELECT o.enabled,o.days,m.enabled,p.revision,(SELECT COALESCE(SUM(bytes),0) FROM archive_usage WHERE viewer=p.viewer AND community=p.community AND agent='') FROM archive_partitions p JOIN archive_subscriptions o USING(viewer,community) JOIN archive_subscriptions m USING(viewer,community) WHERE p.viewer=?1 AND p.community=?2 AND o.name='observer' AND m.name='metrics'",
            params![viewer, community],
            |r| {
                Ok(ArchiveSettings {
                    observer: r.get(0)?,
                    observer_days: r.get(1)?,
                    metrics: r.get(2)?,
                    revision: r.get(3)?,
                    bytes: r.get(4)?,
                })
            },
        ))
    }

    /// Removes expired records across every partition, at most once a
    /// minute.
    fn prune(&mut self, now: i64) -> Result<()> {
        if now < self.pruned + 60 {
            return Ok(());
        }
        database(self.connection.execute(
            "DELETE FROM archive_events WHERE seq IN (SELECT e.seq FROM archive_events e JOIN archive_subscriptions s ON s.viewer=e.viewer AND s.community=e.community AND s.name=e.subscription WHERE e.created <= ?1-s.days*86400)",
            [now],
        ))?;
        let _ = self.vacuum(64);
        self.pruned = now;
        Ok(())
    }

    fn vacuum(&self, pages: u32) -> Result<()> {
        // One row per reclaimed page: drain them all.
        let mut statement = database(
            self.connection
                .prepare(&format!("PRAGMA incremental_vacuum({pages})")),
        )?;
        let mut rows = database(statement.query([]))?;
        while database(rows.next())?.is_some() {}
        Ok(())
    }
}

/// Whether `event` is a fresh agent envelope for `identity` worth
/// archiving (buzz-app's `decode_archive`): the right kind, addressed to
/// the viewer by exactly one `p` tag and from the agent its `agent` tag
/// names, activity marked as telemetry, within size limits, and
/// decrypting to JSON. The signature was verified on receipt.
pub fn check_envelope(identity: &Identity, event: &Event, now: u64) -> Result<()> {
    open_envelope(identity, event, now, FRESHNESS_SECS).map(drop)
}

/// The checks of [`check_envelope`] for an envelope at most `max_age`
/// seconds old; the decrypted frame when it passes.
fn open_envelope(identity: &Identity, event: &Event, now: u64, max_age: u64) -> Result<String> {
    let invalid = || "Invalid observer envelope".to_string();
    let exact = |name: &str, value: &str| {
        let mut tags = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(|tag| tag.first().map(String::as_str) == Some(name));
        tags.next()
            .is_some_and(|tag| tag.len() == 2 && tag[1] == value)
            && tags.next().is_none()
    };
    let kind = event.kind.as_u16();
    let created = event.created_at.as_secs();
    let well_formed = event.tags.len() <= 64
        && event.tags.iter().all(|tag| {
            tag.as_slice().len() <= 16 && tag.as_slice().iter().all(|i| i.len() <= 4096)
        });
    if !well_formed
        || !matches!(kind, kinds::AGENT_ACTIVITY | kinds::AGENT_METRICS)
        || !exact("p", &identity.pubkey_hex())
        || !exact("agent", &event.pubkey.to_hex())
        || (kind == kinds::AGENT_ACTIVITY && !exact("frame", "telemetry"))
        || created > now + 300
        || now.saturating_sub(created) > max_age
        || event.content.len() < MIN_CONTENT
        || event.content.len() > MAX_CONTENT
    {
        return Err(invalid());
    }
    let text = identity
        .decrypt_agent_frame(event)
        .map_err(|_| "Invalid encrypted agent envelope".to_string())?;
    if text.len() > MAX_FRAME || serde_json::from_str::<serde_json::Value>(&text).is_err() {
        return Err("Invalid observer frame".into());
    }
    Ok(text)
}

/// The archive's name for a community: its HTTP origin.
pub fn community_key(origin: &crate::origin::RelayOrigin) -> String {
    origin.http().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp, nip44};

    fn envelope(agent: &Keys, viewer: &Identity, kind: u16, frame: &str, at: u64) -> Event {
        let content = nip44::encrypt(
            agent.secret_key(),
            &viewer.public_key(),
            frame,
            nip44::Version::V2,
        )
        .unwrap();
        let mut tags = vec![
            Tag::parse(["p", &viewer.pubkey_hex()]).unwrap(),
            Tag::parse(["agent", &agent.public_key().to_hex()]).unwrap(),
        ];
        if kind == kinds::AGENT_ACTIVITY {
            tags.push(Tag::parse(["frame", "telemetry"]).unwrap());
        }
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(agent)
            .unwrap()
    }

    #[test]
    fn envelopes_must_be_fresh_addressed_and_decrypt() {
        let viewer = Identity::ephemeral();
        let agent = Keys::generate();
        let now = 1_800_000_000;
        let ok = envelope(&agent, &viewer, 24200, r#"{"type":"turn"}"#, now - 10);
        assert_eq!(check_envelope(&viewer, &ok, now), Ok(()));
        let metrics = envelope(&agent, &viewer, 44200, r#"{"tokens":3}"#, now);
        assert_eq!(check_envelope(&viewer, &metrics, now), Ok(()));
        let stale = envelope(&agent, &viewer, 24200, "{}", now - 301);
        assert!(check_envelope(&viewer, &stale, now).is_err());
        let not_json = envelope(&agent, &viewer, 24200, "not json at all", now);
        assert!(check_envelope(&viewer, &not_json, now).is_err());
        let someone = Identity::ephemeral();
        assert!(
            check_envelope(&someone, &ok, now).is_err(),
            "not addressed to them"
        );
    }

    #[test]
    fn archives_within_settings_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::open(dir.path().join("archive/events.sqlite3")).unwrap();
        let viewer = Identity::ephemeral();
        let (me, community) = (viewer.pubkey_hex(), "https://relay.example.com");
        let settings = archive.settings(&me, community).unwrap();
        assert!(settings.observer && settings.metrics);
        assert_eq!((settings.observer_days, settings.bytes), (30, 0));
        assert_eq!(settings.kinds(), vec![24200, 44200]);

        let agent = Keys::generate();
        let at = now() as u64;
        let activity = envelope(&agent, &viewer, 24200, "{}", at);
        let metrics = envelope(&agent, &viewer, 44200, "{}", at);
        archive
            .ingest(&me, community, &activity, settings.revision)
            .unwrap();
        archive
            .ingest(&me, community, &activity, settings.revision)
            .unwrap();
        archive
            .ingest(&me, community, &metrics, settings.revision)
            .unwrap();
        let saved = archive.settings(&me, community).unwrap();
        assert!(saved.bytes > 0);

        // A configure moves the revision; a capture from before it stops.
        let off = archive
            .configure(&me, community, false, true, 7, saved.revision)
            .unwrap();
        assert_eq!((off.observer, off.observer_days), (false, 7));
        assert_eq!(off.kinds(), vec![44200]);
        assert!(
            archive
                .configure(&me, community, true, true, 7, saved.revision)
                .is_err(),
            "stale revision"
        );
        assert!(
            archive
                .ingest(&me, community, &activity, saved.revision)
                .is_err()
        );
        assert!(
            archive
                .configure(&me, community, true, true, 91, off.revision)
                .is_err()
        );

        // Turning capture off keeps what was saved; clearing removes it.
        assert_eq!(archive.settings(&me, community).unwrap().bytes, saved.bytes);
        archive.clear(&me, community, Some(24200)).unwrap();
        let after = archive.settings(&me, community).unwrap();
        assert!(after.bytes > 0 && after.bytes < saved.bytes, "metrics stay");
        archive.clear(&me, community, None).unwrap();
        assert_eq!(archive.settings(&me, community).unwrap().bytes, 0);
        // Other communities are separate.
        assert_eq!(
            archive
                .settings(&me, "https://other.example.com")
                .unwrap()
                .revision,
            0
        );
    }

    #[test]
    fn reads_pages_newest_first_and_skips_what_no_longer_opens() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::open(dir.path().join("events.sqlite3")).unwrap();
        let viewer = Identity::ephemeral();
        let (me, community) = (viewer.pubkey_hex(), "https://relay.example.com");
        let revision = archive.settings(&me, community).unwrap().revision;
        let (agent, other) = (Keys::generate(), Keys::generate());
        let at = now() as u64;
        for n in 0..(PAGE_SIZE + 5) {
            let frame = format!(r#"{{"n":{n}}}"#);
            let event = envelope(&agent, &viewer, 44200, &frame, at - n as u64);
            archive.ingest(&me, community, &event, revision).unwrap();
        }
        let theirs = envelope(&other, &viewer, 44200, r#"{"n":"other"}"#, at);
        archive.ingest(&me, community, &theirs, revision).unwrap();
        let activity = envelope(&agent, &viewer, 24200, "{}", at);
        archive.ingest(&me, community, &activity, revision).unwrap();

        let first = archive.read(&viewer, community, 44200, None, None).unwrap();
        assert_eq!((first.records.len(), first.skipped), (PAGE_SIZE, 0));
        assert_eq!(first.revision, revision);
        assert_eq!(first.records[0].plaintext, r#"{"n":"other"}"#);
        assert_eq!(first.records[0].agent, other.public_key().to_hex());
        // Newest saved first: by arrival, not creation time.
        assert_eq!(first.records[1].plaintext, r#"{"n":104}"#);
        let second = archive
            .read(&viewer, community, 44200, None, first.before)
            .unwrap();
        assert_eq!((second.records.len(), second.before), (6, None));
        assert_eq!(second.records[5].plaintext, r#"{"n":0}"#);

        let mine = archive
            .read(
                &viewer,
                community,
                44200,
                Some(&other.public_key().to_hex()),
                None,
            )
            .unwrap();
        assert_eq!(mine.records.len(), 1);
        let activity_page = archive.read(&viewer, community, 24200, None, None).unwrap();
        assert_eq!(activity_page.records.len(), 1);
        assert!(archive.read(&viewer, community, 1, None, None).is_err());
        assert!(
            archive
                .read(&viewer, community, 44200, Some("nope"), None)
                .is_err()
        );

        // Someone else reading the same partition opens nothing.
        let stranger = Identity::ephemeral();
        assert_eq!(
            archive
                .read(&stranger, community, 44200, None, None)
                .unwrap()
                .records
                .len(),
            0,
            "partitioned by viewer"
        );
        // A tampered envelope is skipped, not shown.
        {
            let inner = archive.lock().unwrap();
            inner
                .connection
                .execute(
                    "UPDATE archive_events SET envelope=replace(envelope,'\"kind\":44200','\"kind\":44201') WHERE id=?1",
                    [theirs.id.to_hex()],
                )
                .unwrap();
        }
        let tampered = archive.read(&viewer, community, 44200, None, None).unwrap();
        assert_eq!((tampered.records.len(), tampered.skipped), (99, 1));
    }
}
