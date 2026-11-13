//! Durable, bounded refusal notices. Store before sending, preserve the exact
//! signed event after a lost acknowledgement, and retain exhausted records.

use super::Error;
use nostr::{Event, EventId};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILES: usize = 32;
const MAX_BYTES: u64 = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Notice {
    pub trigger: EventId,
    #[serde(default)]
    pub completion: bool,
    pub event: Event,
    pub attempts: u8,
    pub next_try: u64,
}

pub(super) struct Outbox {
    directory: PathBuf,
}

impl Outbox {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
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

    pub fn reserve_completion(&self, trigger: EventId, event: Event) -> Result<(), Error> {
        self.reserve_kind(trigger, event, true)
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
            completion,
            event,
            attempts: 0,
            next_try: 0,
        };
        self.save(&path, &notice, true)
    }

    pub fn next(&self, now: u64) -> Result<Option<(PathBuf, Notice)>, Error> {
        for path in self.files()? {
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .map_err(|_| Error)?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(Error);
            }
            let notice: Notice = serde_json::from_slice(&bytes).map_err(|_| Error)?;
            if path.file_name().and_then(|s| s.to_str())
                != Some(&format!("{}.json", notice.trigger))
            {
                return Err(Error);
            }
            if notice.attempts < 8 && notice.next_try <= now {
                return Ok(Some((path, notice)));
            }
        }
        Ok(None)
    }

    pub fn attempt(&self, path: &Path, notice: &mut Notice, now: u64) -> Result<(), Error> {
        notice.attempts = notice.attempts.saturating_add(1);
        notice.next_try = now.saturating_add((5u64 << notice.attempts.min(4)).min(60));
        self.save(path, notice, false)
    }

    pub fn complete(&self, path: &Path) -> Result<(), Error> {
        fs::remove_file(path).map_err(|_| Error)
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
