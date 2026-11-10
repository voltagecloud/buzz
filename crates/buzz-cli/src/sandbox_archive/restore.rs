//! Explicit cancellation means cancelling the token and awaiting completion.
use super::ArchiveError as Error;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::format;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform;

/// Explicit local limits; configure below the filesystem/container quota.
#[derive(Clone, Copy)]
pub struct RestoreBudget {
    /// Maximum new scratch/import/copy/block usage for this operation.
    pub work_bytes: u64,
    /// Existing store usage plus this operation's peak must fit this limit.
    pub store_bytes: u64,
}
impl Default for RestoreBudget {
    fn default() -> Self {
        Self {
            work_bytes: 128 * 1024 * 1024,
            store_bytes: 128 * 1024 * 1024,
        }
    }
}
/// Atomically published read-only mirror and its repository directories.
pub struct Restored {
    /// Fresh directory which was never previously present.
    pub directory: PathBuf,
    /// Repository roots to open in the explorer for diffs/history.
    pub repositories: Vec<PathBuf>,
}

/// Refuse unsupported platforms or unavailable/denied Git before requesting access.
/// Paseo host-tools=off keeps its existing Git refusal; this function does not bypass it.
pub async fn check_git() -> Result<(), Error> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::check_git().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(Error::Unsupported)
    }
}

/// Restore authenticated plaintext to a fresh directory inside an explicit store.
///
/// Cancel by cancelling `cancel`, then await this future to completion so it can
/// stop/reap Git children and remove staging. Do not abort/drop the future.
/// This local copy remains after grant expiry; remote expiry cannot erase delivered bytes.
pub async fn extract(
    archive: Vec<u8>,
    destination: &Path,
    store_root: &Path,
    budget: RestoreBudget,
    cancel: CancellationToken,
) -> Result<Restored, Error> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::extract(archive, destination, store_root, budget, cancel).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (archive, destination, store_root, budget, cancel);
        Err(Error::Unsupported)
    }
}
