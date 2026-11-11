//! Durable reservations, recoverable upload metadata, and local cleanup intent.

use anyhow::{ensure, Context, Result};
use buzz_sdk::sandbox_attach::{
    snapshot::{self, Failure, Grant},
    Scope,
};
use nostr::{Event, EventId, Keys, PublicKey, Timestamp};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

const MAX_RECORDS: usize = 64;
const MAX_BYTES: u64 = 32768;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) grant: Event,
    pub(super) accepted: Event,
    pub(super) claimed: bool,
    pub(super) metadata: Vec<Event>,
    pub(super) sent: Vec<EventId>,
    pub(super) ended: Option<Event>,
    pub(super) ended_sent: bool,
    pub(super) cleaned: bool,
}

pub(super) struct Journal {
    directory: PathBuf,
    scope: Scope,
    owner: PublicKey,
    _lease: std::fs::File,
}
impl Journal {
    pub(super) fn open(directory: PathBuf, scope: Scope, owner: PublicKey) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join(".lock"))?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("another snapshot service owns this journal")?;
        Ok(Self {
            directory,
            scope,
            owner,
            _lease: lease,
        })
    }
    fn path(&self, id: EventId) -> PathBuf {
        self.directory.join(format!("{}.json", id.to_hex()))
    }
    pub(super) fn job(&self, id: EventId) -> PathBuf {
        self.directory.join(format!("{}.job", id.to_hex()))
    }
    pub(super) fn save(&self, record: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            bytes.len() <= MAX_BYTES as usize,
            "snapshot journal record exceeds bound"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.path(record.grant.id))?;
        std::fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub(super) fn records(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for (count, entry) in std::fs::read_dir(&self.directory)?.enumerate() {
            ensure!(
                count < MAX_RECORDS * 3 + 1,
                "snapshot journal directory exceeds bound"
            );
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".lock" {
                continue;
            }
            if name.ends_with(".job") {
                ensure!(
                    entry.file_type()?.is_dir(),
                    "invalid snapshot job directory"
                );
                continue;
            }
            if name.starts_with(".tmp") {
                ensure!(
                    entry.file_type()?.is_file(),
                    "invalid snapshot staging entry"
                );
                std::fs::remove_file(entry.path())?;
                continue;
            }
            ensure!(
                records.len() < MAX_RECORDS && entry.file_type()?.is_file(),
                "snapshot journal exceeds bound"
            );
            let mut bytes = Vec::new();
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(entry.path())?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= MAX_BYTES as usize,
                "snapshot journal record exceeds bound"
            );
            let record: Record = serde_json::from_slice(&bytes)?;
            ensure!(
                entry.path() == self.path(record.grant.id)
                    && record.metadata.len() <= 2
                    && record.sent.len() <= 2,
                "snapshot journal shape invalid"
            );
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            ensure!(
                record.grant.pubkey == self.owner
                    && record.accepted.verify().is_ok()
                    && record.accepted.pubkey == self.scope.agent
                    && buzz_core::sandbox_attach::acceptance_coordinate(&record.accepted)
                        == Some(("snapshot", grant.session())),
                "snapshot journal authority invalid"
            );
            for event in &record.metadata {
                snapshot::upload::Upload::parse(event, self.scope, self.owner)?;
            }
            if let Some(ended) = &record.ended {
                snapshot::End::parse(ended, &grant)?;
            }
            records.push(record);
        }
        Ok(records)
    }
    pub(super) fn reserve(&self, keys: &Keys, grant: &Grant) -> Result<Record> {
        ensure!(
            !self.path(grant.id()).try_exists()? && self.records()?.len() < MAX_RECORDS,
            "snapshot approval consumed or journal full"
        );
        let record = Record {
            grant: grant.event().clone(),
            accepted: snapshot::audit(keys, grant, "accepted", 0, None)?,
            claimed: false,
            metadata: Vec::new(),
            sent: Vec::new(),
            ended: None,
            ended_sent: false,
            cleaned: false,
        };
        self.save(&record)?;
        Ok(record)
    }
    pub(super) fn prepare(&self, record: &Record) -> Result<()> {
        let path = self.job(record.grant.id);
        std::fs::create_dir(&path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        std::fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub(super) fn finish(
        &self,
        keys: &Keys,
        record: &mut Record,
        bytes: u64,
        reason: Option<Failure>,
    ) -> Result<()> {
        if record.claimed && record.ended.is_none() {
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            record.ended = Some(snapshot::audit(keys, &grant, "ended", bytes, reason)?);
            self.save(record)?;
        }
        Ok(())
    }
    pub(super) fn cleanup(&self, record: &mut Record) -> Result<()> {
        if record.cleaned {
            return Ok(());
        }
        let path = self.job(record.grant.id);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "invalid snapshot cleanup path"
                );
                std::fs::remove_dir_all(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        record.cleaned = true;
        self.save(record)
    }
    pub(super) fn recover(&self, keys: &Keys) -> Result<()> {
        for mut record in self.records()? {
            self.finish(keys, &mut record, 0, Some(Failure::Archive))?;
            self.cleanup(&mut record)?;
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            if grant.expires_at() <= Timestamp::now().as_secs()
                && (!record.claimed || record.ended_sent)
                && record
                    .metadata
                    .iter()
                    .all(|event| record.sent.contains(&event.id))
            {
                std::fs::remove_file(self.path(record.grant.id))?;
                std::fs::File::open(&self.directory)?.sync_all()?;
            }
        }
        Ok(())
    }
}
