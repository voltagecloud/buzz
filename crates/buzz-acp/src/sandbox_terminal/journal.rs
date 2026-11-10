//! Durable single-use reservation and metadata-only audit outbox.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use buzz_sdk::sandbox_attach::{audit, Grant, Scope};
use nostr::{Event, EventId, Keys, PublicKey, Timestamp};
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 256;
const MAX_RECORD_BYTES: u64 = 16 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) grant: Event,
    pub(super) accepted: Event,
    pub(super) ended: Option<Event>,
    /// Only the exact first-claim receipt can set this before worker startup.
    pub(super) claimed: bool,
    pub(super) ended_sent: bool,
    pub(super) bytes_in: u64,
    pub(super) bytes_out: u64,
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
            .context("another terminal server owns this sandbox journal")?;
        for (count, entry) in std::fs::read_dir(&directory)?.enumerate() {
            ensure!(
                count < MAX_RECORDS * 4 + 1,
                "terminal journal directory exceeds bound"
            );
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(".tmp") {
                ensure!(
                    entry.file_type()?.is_file(),
                    "invalid terminal journal staging file"
                );
                std::fs::remove_file(entry.path())?;
            }
        }
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

    pub(super) fn save(&self, record: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES as usize,
            "terminal journal record exceeds bound"
        );
        let mut staged = tempfile::NamedTempFile::new_in(&self.directory)?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(self.path(record.grant.id))?;
        std::fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub(super) fn records(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.file_name() == ".lock" {
                continue;
            }
            ensure!(
                records.len() < MAX_RECORDS,
                "terminal audit journal is full"
            );
            ensure!(
                entry.file_type()?.is_file(),
                "invalid terminal journal entry"
            );
            let mut bytes = Vec::new();
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(entry.path())?
                .take(MAX_RECORD_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= MAX_RECORD_BYTES as usize,
                "terminal journal record exceeds bound"
            );
            let record: Record = serde_json::from_slice(&bytes)?;
            ensure!(
                entry.path() == self.path(record.grant.id),
                "terminal journal coordinate mismatch"
            );
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            ensure!(
                record.grant.verify().is_ok()
                    && record.accepted.verify().is_ok()
                    && record
                        .ended
                        .as_ref()
                        .is_none_or(|event| event.verify().is_ok()),
                "terminal journal signature invalid"
            );
            ensure!(
                record.grant.pubkey == self.owner
                    && record.accepted.pubkey == self.scope.agent
                    && buzz_core::sandbox_attach::acceptance_coordinate(&record.accepted)
                        == Some(("terminal", grant.session()))
                    && record
                        .ended
                        .as_ref()
                        .is_none_or(|event| event.pubkey == self.scope.agent),
                "terminal journal authority mismatch"
            );
            records.push(record);
        }
        Ok(records)
    }

    /// An existing record consumes this approval, including after a crash or a
    /// lost relay receipt. A new shell always requires a new owner approval.
    pub(super) fn reserve(&self, keys: &Keys, grant: &Grant) -> Result<Record> {
        ensure!(
            !self.path(grant.id()).try_exists()?,
            "terminal approval was already consumed"
        );
        ensure!(
            self.records()?.len() < MAX_RECORDS,
            "terminal audit journal is full"
        );
        let record = Record {
            grant: grant.event().clone(),
            accepted: audit(keys, grant, "accepted", 0, 0)?,
            ended: None,
            claimed: false,
            ended_sent: false,
            bytes_in: 0,
            bytes_out: 0,
        };
        self.save(&record)?;
        Ok(record)
    }

    pub(super) fn finish(&self, keys: &Keys, record: &mut Record) -> Result<()> {
        // An unconfirmed or losing claim must not publish an end record that
        // appears to end another replica's successful attempt.
        if record.claimed && record.ended.is_none() {
            // Recovery may finish a grant after expiry; parse at its creation
            // time solely to rebuild audit metadata, never to reopen a shell.
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            record.ended = Some(audit(
                keys,
                &grant,
                "ended",
                record.bytes_in,
                record.bytes_out,
            )?);
            self.save(record)?;
        }
        Ok(())
    }

    pub(super) fn recover(&self, keys: &Keys) -> Result<()> {
        for mut record in self.records()? {
            self.finish(keys, &mut record)?;
            let grant = Grant::parse(&record.grant, self.scope, record.grant.created_at.as_secs())?;
            if (!record.claimed || record.ended_sent)
                && grant.expires_at() <= Timestamp::now().as_secs()
            {
                std::fs::remove_file(self.path(record.grant.id))?;
                std::fs::File::open(&self.directory)?.sync_all()?;
            }
        }
        Ok(())
    }
}
