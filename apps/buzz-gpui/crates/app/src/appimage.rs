//! Whether this copy runs from an AppImage, and which file that is.
//!
//! The AppImage runtime mounts the image at a temporary directory, sets
//! `APPDIR` to it and `APPIMAGE` to the image file, then starts the binary
//! inside. The mount goes away when the app exits, so whatever outlives this
//! process (the `buzz://` handler, the next version's download) names the
//! image, never the running binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The AppImage file this process runs from, if it runs from one.
pub fn path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // The executable's path is canonical; the mount's is as the runtime
    // built it from TMPDIR, which can run through a symlink.
    let appdir = std::env::var_os("APPDIR")
        .map(|dir| std::fs::canonicalize(&dir).map_or(dir, PathBuf::into_os_string));
    from_env(std::env::var_os("APPIMAGE"), appdir, &exe)
}

/// `appimage`, when `exe` lies inside `appdir`. A program started from
/// inside some other AppImage (a terminal, say) inherits both variables,
/// but its binary is elsewhere.
fn from_env(appimage: Option<OsString>, appdir: Option<OsString>, exe: &Path) -> Option<PathBuf> {
    let appimage = PathBuf::from(appimage?);
    let appdir = PathBuf::from(appdir?);
    (appimage.is_absolute() && appdir.is_absolute() && exe.starts_with(&appdir)).then_some(appimage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(appimage: Option<&str>, appdir: Option<&str>, exe: &str) -> Option<PathBuf> {
        from_env(
            appimage.map(Into::into),
            appdir.map(Into::into),
            Path::new(exe),
        )
    }

    #[test]
    fn names_the_image_the_binary_runs_from() {
        assert_eq!(
            env(
                Some("/home/me/Apps/Buzz.AppImage"),
                Some("/tmp/.mount_BuzzAb12"),
                "/tmp/.mount_BuzzAb12/usr/bin/buzz-gpui",
            ),
            Some(PathBuf::from("/home/me/Apps/Buzz.AppImage"))
        );
    }

    #[test]
    fn ignores_variables_inherited_from_another_image() {
        assert_eq!(
            env(
                Some("/home/me/Apps/Terminal.AppImage"),
                Some("/tmp/.mount_TermCd34"),
                "/usr/bin/buzz-gpui",
            ),
            None
        );
    }

    #[test]
    fn needs_both_variables_as_absolute_paths() {
        let exe = "/tmp/.mount_BuzzAb12/usr/bin/buzz-gpui";
        assert_eq!(env(None, Some("/tmp/.mount_BuzzAb12"), exe), None);
        assert_eq!(env(Some("/home/me/Buzz.AppImage"), None, exe), None);
        assert_eq!(
            env(Some("Buzz.AppImage"), Some("/tmp/.mount_BuzzAb12"), exe),
            None
        );
        assert_eq!(env(Some("/home/me/Buzz.AppImage"), Some(""), exe), None);
    }
}
