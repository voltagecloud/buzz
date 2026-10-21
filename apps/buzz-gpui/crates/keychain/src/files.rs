//! Files only the current user may open, for secrets kept outside the OS
//! store: tokens, settings that hold them, and agent keys on hosts with no
//! store.

use std::io::{self, Write};
use std::path::Path;

/// Replaces `path` with `contents`, readable and writable by the owner
/// only, an existing file included.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // The mode above applies only to a file it creates.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()
}

/// Creates `path` and its parents, and lets only the owner into it.
pub fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_files_and_directories_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("a/b");
        private_dir(&inner).unwrap();
        let file = inner.join("token");
        std::fs::write(&file, "old").unwrap();
        write_private(&file, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&inner), 0o700);
            assert_eq!(mode(&file), 0o600);
        }
    }
}
