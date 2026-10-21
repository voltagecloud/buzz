//! Replacing this copy: only a Linux install from the .deb can, by
//! handing the downloaded package to apt with an administrator prompt
//! (polkit's `pkexec`), and then starting the new binary once this
//! process has gone.

/// How this copy can be updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed from the .deb: download, verify and install in place.
    Deb,
    /// A packaged release elsewhere (macOS, Windows): the browser
    /// downloads the new one.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    Browser,
    /// Built from source: only say that a release exists.
    Dev,
}

/// Where the .deb puts the binary.
#[cfg(target_os = "linux")]
pub const DEB_BINARY: &str = "/usr/bin/buzz-gpui";
/// The package's file list, which dpkg keeps for each installed package.
#[cfg(target_os = "linux")]
const DEB_FILE_LIST: &str = "/var/lib/dpkg/info/buzz-gpui.list";

impl InstallKind {
    pub fn detect() -> Self {
        // Development runs can act as the .deb to try the whole flow.
        if cfg!(debug_assertions)
            && std::env::var("BUZZ_UPDATE_KIND").is_ok_and(|kind| kind == "deb")
        {
            return Self::Deb;
        }
        if cfg!(debug_assertions) {
            return Self::Dev;
        }
        Self::detect_release()
    }

    #[cfg(target_os = "linux")]
    fn detect_release() -> Self {
        let exe = std::env::current_exe().ok();
        let from_deb = exe.is_some_and(|exe| running_binary(&exe) == DEB_BINARY)
            && std::fs::read_to_string(DEB_FILE_LIST).is_ok_and(|list| lists_binary(&list));
        if from_deb { Self::Deb } else { Self::Dev }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn detect_release() -> Self {
        Self::Browser
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn detect_release() -> Self {
        Self::Dev
    }
}

/// The binary's path, without the " (deleted)" Linux adds once a package
/// upgrade has replaced it.
#[cfg(target_os = "linux")]
fn running_binary(exe: &std::path::Path) -> &str {
    let path = exe.to_str().unwrap_or_default();
    path.strip_suffix(" (deleted)").unwrap_or(path)
}

#[cfg(target_os = "linux")]
fn lists_binary(list: &str) -> bool {
    list.lines().any(|line| line.trim() == DEB_BINARY)
}

/// The command that installs `package`: apt resolves any new
/// dependencies; without it, dpkg. Not root, so polkit asks for a
/// password first.
#[cfg(target_os = "linux")]
pub fn install_command(
    package: &std::path::Path,
    root: bool,
    have_apt: bool,
) -> (String, Vec<String>) {
    let package = package.to_string_lossy().into_owned();
    let mut argv: Vec<String> = if have_apt {
        vec!["apt-get".into(), "install".into(), "-y".into(), package]
    } else {
        vec!["dpkg".into(), "-i".into(), package]
    };
    if !root {
        argv.insert(0, "pkexec".into());
    }
    let program = argv.remove(0);
    (program, argv)
}

/// Installs `package`, blocking until apt is done. The error is short
/// enough to show.
#[cfg(target_os = "linux")]
pub fn install_deb(package: &std::path::Path) -> Result<(), String> {
    let root = unsafe { libc::geteuid() } == 0;
    let have_apt = std::path::Path::new("/usr/bin/apt-get").exists();
    let (program, args) = match std::env::var("BUZZ_UPDATE_INSTALLER") {
        // Development runs can stand in for apt, to try the flow on a
        // machine without it.
        Ok(installer) if cfg!(debug_assertions) => {
            (installer, vec![package.to_string_lossy().into_owned()])
        }
        _ => install_command(package, root, have_apt),
    };
    tracing::info!(%program, ?args, "installing the update");
    let output = std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .map_err(|error| format!("Couldn't run {program}: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        tracing::info!(stdout = %String::from_utf8_lossy(&output.stdout), "update installed");
        return Ok(());
    }
    tracing::error!(status = ?output.status, %stderr, "update install failed");
    Err(match output.status.code() {
        // pkexec: the prompt was dismissed, or the password refused.
        Some(126) if !root => "The administrator prompt was dismissed.".into(),
        Some(127) if !root && stderr.contains("Not authorized") => {
            "Buzz isn't allowed to install updates on this computer.".into()
        }
        _ => {
            let last = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no details");
            format!("Installing failed: {last}")
        }
    })
}

#[cfg(not(target_os = "linux"))]
pub fn install_deb(_package: &std::path::Path) -> Result<(), String> {
    Err("Updates install only on Linux".into())
}

/// The installed package's version, as dpkg reports it.
#[cfg(target_os = "linux")]
pub fn installed_version() -> Option<String> {
    let output = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "buzz-gpui"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn installed_version() -> Option<String> {
    None
}

/// Starts `program` once this process has exited, so the new copy does
/// not hand itself to this one over the single-instance socket. A shell
/// waits on the PID, in its own process group so it outlives us; it
/// gives up waiting after a minute.
#[cfg(unix)]
pub fn relaunch_after_exit(program: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    const SCRIPT: &str = "i=0; while kill -0 \"$1\" 2>/dev/null && [ $i -lt 300 ]; do \
                          sleep 0.2; i=$((i+1)); done; exec \"$2\"";
    std::process::Command::new("/bin/sh")
        .args(["-c", SCRIPT, "buzz-relaunch"])
        .arg(std::process::id().to_string())
        .arg(program)
        // Stale for the next launch; the launcher gives it a new one.
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
        .map_err(|error| format!("Couldn't start Buzz again: {error}"))
}

#[cfg(not(unix))]
pub fn relaunch_after_exit(_program: &str) -> Result<(), String> {
    Err("Restart Buzz to finish the update".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::path::Path;

    #[test]
    fn installs_through_apt_with_a_prompt() {
        let package = Path::new("/home/me/.cache/buzz-gpui/updates/buzz-gpui_0.1.1-1_amd64.deb");
        let (program, args) = install_command(package, false, true);
        assert_eq!(program, "pkexec");
        assert_eq!(
            args,
            [
                "apt-get",
                "install",
                "-y",
                "/home/me/.cache/buzz-gpui/updates/buzz-gpui_0.1.1-1_amd64.deb"
            ]
        );
        let (program, args) = install_command(package, true, false);
        assert_eq!(program, "dpkg");
        assert_eq!(args[0], "-i");
    }

    #[test]
    fn recognises_the_package_binary() {
        assert_eq!(
            running_binary(Path::new("/usr/bin/buzz-gpui (deleted)")),
            DEB_BINARY
        );
        assert_eq!(running_binary(Path::new("/usr/bin/buzz-gpui")), DEB_BINARY);
        assert!(lists_binary(
            "/.\n/usr\n/usr/bin\n/usr/bin/buzz-gpui\n/usr/share/applications/buzz-gpui.desktop\n"
        ));
        assert!(!lists_binary("/usr/bin/buzz-gpui-helper\n"));
    }
}
