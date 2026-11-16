//! Durable, bounded refusal notices. Store before sending, preserve the exact
//! signed event after a lost acknowledgement, and retain exhausted records.

use super::Error;
use nostr::{Event, EventId};
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILES: usize = 32;
const MAX_BYTES: u64 = 4096;

#[derive(Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Pending,
    #[default]
    Ready,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReservationError {
    #[error("handoff completion capacity is full; retry this task")]
    Full,
    #[error("handoff completion storage is unavailable; retry this task")]
    Storage,
    #[error("handoff completion is pending recovery; retry this task")]
    Recovering,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Notice {
    pub trigger: EventId,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub lookup_failures: u8,
    #[serde(default)]
    pub completion: bool,
    pub event: Event,
    pub attempts: u8,
    pub next_try: u64,
}

pub(super) struct Outbox {
    directory: PathBuf,
    owned: HashSet<EventId>,
    queued: HashSet<EventId>,
    cursor: Cell<usize>,
}

impl Outbox {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            owned: HashSet::new(),
            queued: HashSet::new(),
            cursor: Cell::new(0),
        }
    }

    fn files(&self) -> Result<Vec<PathBuf>, Error> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.directory)
            .map_err(|_| Error)?
            .take(MAX_FILES + 1)
        {
            let entry = entry.map_err(|_| Error)?;
            if !entry.file_type().map_err(|_| Error)?.is_file() {
                return Err(Error);
            }
            files.push(entry.path());
        }
        if files.len() > MAX_FILES {
            return Err(Error);
        }
        files.sort();
        Ok(files)
    }

    pub fn reserve(&self, trigger: EventId, event: Event) -> Result<(), Error> {
        self.reserve_kind(trigger, event, false)
    }

    pub fn reserve_completion(
        &mut self,
        trigger: EventId,
        event: Event,
    ) -> Result<(), ReservationError> {
        let path = self.directory.join(format!("{trigger}.json"));
        if self.owned.contains(&trigger) {
            self.queued.remove(&trigger);
            return Ok(());
        }
        if path.try_exists().map_err(|_| ReservationError::Storage)? {
            return Err(ReservationError::Recovering);
        }
        fs::create_dir_all(&self.directory).map_err(|_| ReservationError::Storage)?;
        if self.files().map_err(|_| ReservationError::Storage)?.len() >= MAX_FILES {
            return Err(ReservationError::Full);
        }
        self.reserve_kind(trigger, event, true)
            .map_err(|_| ReservationError::Storage)?;
        self.owned.insert(trigger);
        Ok(())
    }

    pub fn release_completion(&mut self, trigger: EventId, event: Event) -> Result<(), Error> {
        if !self.owned.contains(&trigger) {
            return Ok(());
        }
        let path = self.directory.join(format!("{trigger}.json"));
        // Remove the live lease even if updating the record fails. The original
        // signed fallback stays recoverable on disk and may be reconciled.
        self.owned.remove(&trigger);
        self.queued.remove(&trigger);
        let mut notice = self.load(&path)?;
        notice.phase = Phase::Ready;
        notice.event = event;
        self.save(&path, &notice, false)
    }

    pub fn mark_queued(&mut self, ids: &[EventId]) {
        self.queued
            .extend(ids.iter().filter(|id| self.owned.contains(id)).copied());
    }

    pub fn recover_abandoned(&mut self, retained: impl Fn(EventId) -> bool) -> Result<(), Error> {
        let abandoned: Vec<_> = self
            .queued
            .iter()
            .copied()
            .filter(|id| !retained(*id))
            .collect();
        for trigger in abandoned {
            let path = self.directory.join(format!("{trigger}.json"));
            let notice = self.load(&path)?;
            self.release_completion(trigger, notice.event)?;
        }
        Ok(())
    }

    fn reserve_kind(&self, trigger: EventId, event: Event, completion: bool) -> Result<(), Error> {
        fs::create_dir_all(&self.directory).map_err(|_| Error)?;
        let path = self.directory.join(format!("{trigger}.json"));
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            return if metadata.file_type().is_file() {
                Ok(())
            } else {
                Err(Error)
            };
        }
        if self.files()?.len() >= MAX_FILES {
            return Err(Error);
        }
        let notice = Notice {
            trigger,
            phase: if completion {
                Phase::Pending
            } else {
                Phase::Ready
            },
            lookup_failures: 0,
            completion,
            event,
            attempts: 0,
            next_try: 0,
        };
        self.save(&path, &notice, true)
    }

    pub fn next(&self, now: u64) -> Result<Option<(PathBuf, Notice)>, Error> {
        let files = self.files()?;
        let count = files.len();
        for offset in 0..count {
            let index = (self.cursor.get() + offset) % count;
            let path = &files[index];
            let notice = self.load(path)?;
            if self.owned.contains(&notice.trigger) {
                continue;
            }
            if (notice.completion || notice.attempts < 8) && notice.next_try <= now {
                self.cursor.set((index + 1) % count);
                return Ok(Some((path.clone(), notice)));
            }
        }
        Ok(None)
    }

    fn load(&self, path: &Path) -> Result<Notice, Error> {
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(|_| Error)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error);
        }
        let notice: Notice = serde_json::from_slice(&bytes).map_err(|_| Error)?;
        if path.file_name().and_then(|s| s.to_str()) != Some(&format!("{}.json", notice.trigger)) {
            return Err(Error);
        }
        Ok(notice)
    }

    pub fn lookup_retry(&self, path: &Path, notice: &mut Notice, now: u64) -> Result<(), Error> {
        notice.lookup_failures = notice.lookup_failures.saturating_add(1);
        notice.next_try = now.saturating_add((5u64 << notice.lookup_failures.min(4)).min(60));
        self.save(path, notice, false)
    }

    pub fn attempt(&self, path: &Path, notice: &mut Notice, now: u64) -> Result<(), Error> {
        notice.phase = Phase::Ready;
        notice.lookup_failures = 0;
        notice.attempts = notice.attempts.saturating_add(1);
        notice.next_try = now.saturating_add((5u64 << notice.attempts.min(4)).min(60));
        self.save(path, notice, false)
    }

    pub fn complete(&self, path: &Path) -> Result<(), Error> {
        fs::remove_file(path).map_err(|_| Error)?;
        #[cfg(unix)]
        fs::File::open(&self.directory)
            .map_err(|_| Error)?
            .sync_all()
            .map_err(|_| Error)?;
        Ok(())
    }

    fn save(&self, path: &Path, notice: &Notice, create: bool) -> Result<(), Error> {
        let data = serde_json::to_vec(notice).map_err(|_| Error)?;
        if data.len() as u64 > MAX_BYTES {
            return Err(Error);
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| Error)?;
        file.write_all(&data).map_err(|_| Error)?;
        file.as_file().sync_all().map_err(|_| Error)?;
        if create {
            if let Err(error) = file.persist_noclobber(path) {
                if error.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(Error);
                }
            }
        } else {
            file.persist(path).map_err(|_| Error)?;
        }
        #[cfg(unix)]
        fs::File::open(&self.directory)
            .map_err(|_| Error)?
            .sync_all()
            .map_err(|_| Error)?;
        Ok(())
    }
}
