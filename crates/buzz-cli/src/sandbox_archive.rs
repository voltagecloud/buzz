//! Local snapshot preflight and cancellation-aware read-only restoration.
use std::fs::OpenOptions;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use buzz_sdk::sandbox_attach::snapshot::{
    archive::inspect_bundle, MAX_BLOB_BYTES, MAX_SOURCE_BYTES,
};
use serde::{Deserialize, Serialize};

mod format;
pub use format::{inspect_archive, ArchivePlan};

/// Local archive refusal; file contents and scanner diagnostics are never included.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// Cancellation completed after staging and child cleanup.
    #[error("snapshot extraction cancelled")]
    Cancelled,
    /// Filesystem access failed.
    #[error("archive file access failed: {0}")]
    Io(#[from] std::io::Error),
    /// A hard file/type or metadata bound was exceeded.
    #[error("{0}")]
    Invalid(&'static str),
    /// The SDK rejected malformed or oversized bundle contents.
    #[error("{0}")]
    Bundle(#[from] buzz_sdk::SdkError),
}

/// Bounded admission metadata for the exact captured bundle.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSummary {
    /// Local helper format version.
    pub version: u8,
    /// Number of packed objects, checked before import.
    pub objects: u32,
    /// Sum of reconstructed object bytes, including delta targets.
    pub expanded_bytes: u64,
    /// Object-ID length in bytes (20 or 32).
    pub hash_bytes: usize,
    /// Object ID/ref name pairs; callers account for ref/index/block costs.
    pub refs: Vec<(String, String)>,
}

/// Inspect a regular file without allocating from unchecked file metadata.
/// The bounded descriptor read also catches growth after the initial metadata check.
pub fn inspect_bundle_file(path: &Path) -> Result<BundleSummary, ArchiveError> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(ArchiveError::Invalid("bundle must be a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BLOB_BYTES as u64 {
        return Err(ArchiveError::Invalid(
            "bundle exceeds regular-file byte budget",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BLOB_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(ArchiveError::Invalid("bundle grew beyond byte budget"));
    }
    let inventory = inspect_bundle(&bytes, MAX_SOURCE_BYTES)?;
    Ok(BundleSummary {
        version: 1,
        objects: inventory.objects,
        expanded_bytes: inventory.expanded_bytes,
        hash_bytes: inventory.hash_bytes,
        refs: inventory.refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspector_refuses_oversized_files_before_reading_and_nonregular_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sparse.bundle");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BLOB_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            inspect_bundle_file(&path),
            Err(ArchiveError::Invalid(_))
        ));
        assert!(inspect_bundle_file(temp.path()).is_err());
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(inspect_bundle_file(&link).is_err());
        }
    }
}
