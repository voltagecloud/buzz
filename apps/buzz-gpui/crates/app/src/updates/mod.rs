//! In-app updates from GitHub releases (buzz-app's `updates.ts`, with
//! GitHub in place of the Tauri updater).
//!
//! A check runs shortly after launch and every six hours while automatic
//! checks are on, and whenever Settings → Updates asks. A newer release
//! is offered by a toast and in Settings. A copy installed from the .deb
//! downloads the package, checks it against the release's `SHA256SUMS`,
//! installs it through apt behind an administrator prompt and offers to
//! restart. Other packaged copies send the browser to the download;
//! development builds only say a release exists. Nothing here can stop
//! the app: failures are logged, and shown quietly in Settings.

pub mod fetch;
pub mod install;
pub mod release;

use crate::community::AppState;
use crate::runtime::Runtime;
use crate::ui::toasts::{self, Action, Notice};
use buzz_ui::ToastTone;
use gpui_kit::*;
pub use install::InstallKind;
use release::{Platform, Release};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The latest published release; GitHub leaves drafts and prereleases
/// out of it.
const LATEST_URL: &str = "https://api.github.com/repos/benthecarman/buzz-gpui/releases/latest";
/// Development builds can point checks at a stand-in server.
const URL_OVERRIDE: &str = "BUZZ_UPDATE_URL";
/// Device preference holding [`Preferences`].
const PREFERENCES: &str = "updates";
/// The first check waits until launch has settled.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(15);
/// buzz-app's `BACKGROUND_UPDATE_CHECK_INTERVAL_MS`.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// The toast's key: each state replaces the last.
pub const TOAST: &str = "update";

actions!(
    buzz,
    [
        /// Open Settings → Updates.
        OpenUpdateSettings,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Check at launch and every few hours.
    pub automatic: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { automatic: true }
    }
}

/// buzz-app's `UpdateStatus`, with the install steps this port adds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// A newer release is out; see [`Updater::release`].
    Available,
    Downloading,
    /// Downloaded and verified; installing is one click away.
    Ready,
    Installing,
    /// Installed; restarting runs it.
    Installed,
    Error(SharedString),
}

pub struct Updater {
    status: Status,
    /// The newer release, once one is found.
    release: Option<Release>,
    /// The verified package for [`Self::release`], and its digest so it is
    /// checked again just before it installs.
    package: Option<(PathBuf, [u8; 32])>,
    kind: InstallKind,
    platform: Platform,
    preferences: Preferences,
    current: semver::Version,
    url: String,
    client: reqwest::Client,
    cache: Arc<Mutex<fetch::Cache>>,
    /// Bytes downloaded so far, and the size expected.
    progress: Arc<AtomicU64>,
    expected: u64,
    checking: bool,
    /// The version a toast was raised for, so later checks don't raise it
    /// again.
    announced: Option<semver::Version>,
}

impl Global for Updater {}

pub fn init(cx: &mut App) {
    let preferences = cx
        .global::<AppState>()
        .store
        .device_pref(PREFERENCES)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let url = match std::env::var(URL_OVERRIDE) {
        Ok(url) if cfg!(debug_assertions) => url,
        _ => LATEST_URL.to_string(),
    };
    let kind = InstallKind::detect();
    tracing::info!(?kind, %url, "updates");
    cx.set_global(Updater {
        status: Status::Idle,
        release: None,
        package: None,
        kind,
        platform: Platform::current(),
        preferences,
        current: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("a semver version"),
        url,
        client: fetch::client(),
        cache: Arc::default(),
        progress: Arc::default(),
        expected: 0,
        checking: false,
        announced: None,
    });
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK_AFTER).await;
        loop {
            cx.update(|cx| {
                if cx.global::<Updater>().preferences.automatic {
                    check(false, cx);
                }
            });
            cx.background_executor().timer(CHECK_EVERY).await;
        }
    })
    .detach();
}

/// Only HTTPS, except from a development build's stand-in server.
pub(crate) fn allowed_url(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://"))
}

impl Updater {
    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn release(&self) -> Option<&Release> {
        self.release.as_ref()
    }

    pub fn kind(&self) -> InstallKind {
        self.kind
    }

    pub fn preferences(&self) -> Preferences {
        self.preferences
    }

    /// The newer version on offer.
    pub fn version(&self) -> Option<semver::Version> {
        self.release.as_ref().and_then(Release::version)
    }

    /// How far the download is, from 0 to 1, when its size is known.
    pub fn fraction(&self) -> Option<f32> {
        (self.status == Status::Downloading && self.expected > 0)
            .then(|| (self.progress.load(Ordering::Relaxed) as f32 / self.expected as f32).min(1.))
    }

    /// Where the browser should go for this platform's package.
    pub fn download_url(&self) -> Option<String> {
        let release = self.release.as_ref()?;
        Some(
            release
                .asset_for(self.platform)
                .map(|asset| asset.browser_download_url.clone())
                .unwrap_or_else(|| release.html_url.clone()),
        )
    }
}

fn set_status(status: Status, cx: &mut App) {
    cx.global_mut::<Updater>().status = status;
    cx.refresh_windows();
}

pub fn set_preferences(preferences: Preferences, cx: &mut App) {
    let was = cx.global::<Updater>().preferences;
    cx.global_mut::<Updater>().preferences = preferences;
    let json = serde_json::to_string(&preferences).expect("preferences serialize");
    if let Err(error) = cx
        .global::<AppState>()
        .store
        .set_device_pref(PREFERENCES, &json)
    {
        tracing::warn!(%error, "could not save the update settings");
    }
    if preferences.automatic && !was.automatic {
        check(false, cx);
    }
    cx.refresh_windows();
}

/// Asks GitHub for the latest release. A background check stays quiet
/// unless it finds one, and leaves a download or install alone.
pub fn check(manual: bool, cx: &mut App) {
    let updater = cx.global::<Updater>();
    if updater.checking {
        if manual {
            set_status(Status::Checking, cx);
        }
        return;
    }
    if matches!(
        updater.status,
        Status::Downloading | Status::Installing | Status::Installed
    ) || (!manual && updater.status == Status::Ready)
    {
        return;
    }
    let (client, url, cache) = (
        updater.client.clone(),
        updater.url.clone(),
        updater.cache.clone(),
    );
    cx.global_mut::<Updater>().checking = true;
    if manual {
        set_status(Status::Checking, cx);
    }
    let task = Runtime::handle(cx).spawn(async move { fetch::latest(&client, &url, &cache).await });
    cx.spawn(async move |cx| {
        let result = match task.await {
            Ok(result) => result,
            Err(error) => Err(fetch::CheckError::Failed(error.to_string())),
        };
        cx.update(|cx| checked(result, manual, cx));
    })
    .detach();
}

fn checked(result: Result<Option<Release>, fetch::CheckError>, manual: bool, cx: &mut App) {
    let updater = cx.global_mut::<Updater>();
    updater.checking = false;
    // A manual check that joined this one shows its answer too.
    let manual = manual || updater.status == Status::Checking;
    let release = match result {
        Ok(release) => release,
        Err(error) => {
            tracing::warn!(%error, "update check failed");
            if manual {
                set_status(Status::Error(error.to_string().into()), cx);
            }
            return;
        }
    };
    let newer = release.filter(|release| release.newer_than(&updater.current));
    let Some(release) = newer else {
        tracing::info!("no newer release");
        updater.release = None;
        updater.package = None;
        set_status(Status::UpToDate, cx);
        return;
    };
    let same = updater.version() == release.version() && updater.package.is_some();
    tracing::info!(tag = %release.tag_name, "a newer release is out");
    updater.release = Some(release);
    if same {
        set_status(Status::Ready, cx);
    } else {
        updater.package = None;
        match updater.kind {
            InstallKind::Deb => download(cx),
            InstallKind::Browser | InstallKind::Dev => set_status(Status::Available, cx),
        }
    }
    announce(cx);
}

/// Fetches and verifies the package for the release on offer.
fn download(cx: &mut App) {
    let updater = cx.global_mut::<Updater>();
    let Some(release) = updater.release.clone() else {
        return;
    };
    let Some(asset) = release.asset_for(updater.platform).cloned() else {
        set_status(
            Status::Error("This release has no package for this computer.".into()),
            cx,
        );
        return;
    };
    let Some(sums) = release.asset(release::CHECKSUMS).cloned() else {
        set_status(
            Status::Error("This release has no checksums, so it can't be installed safely.".into()),
            cx,
        );
        return;
    };
    let Some(dir) = dirs::cache_dir().map(|dir| dir.join("buzz-gpui/updates")) else {
        set_status(
            Status::Error("No cache directory to download to.".into()),
            cx,
        );
        return;
    };
    updater.expected = asset.size;
    updater.progress.store(0, Ordering::Relaxed);
    let (client, progress) = (updater.client.clone(), updater.progress.clone());
    set_status(Status::Downloading, cx);
    let task = Runtime::handle(cx).spawn(async move {
        let text = fetch::text(&client, &sums.browser_download_url).await?;
        let expected = release::checksum_for(&text, &asset.name)
            .ok_or_else(|| format!("{} isn't in the release's checksums.", asset.name))?;
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("Couldn't save the update: {error}"))?;
        let dest = dir.join(&asset.name);
        // Packages from earlier offers go.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path() != dest {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        if release::verify(&dest, &expected).is_ok() {
            return Ok((dest, expected));
        }
        fetch::download(&client, &asset.browser_download_url, &dest, &progress).await?;
        if let Err(error) = release::verify(&dest, &expected) {
            let _ = std::fs::remove_file(&dest);
            return Err(error);
        }
        Ok((dest, expected))
    });
    // The percentage in Settings.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let downloading = cx.update(|cx| {
                let downloading = cx.global::<Updater>().status == Status::Downloading;
                if downloading {
                    cx.refresh_windows();
                }
                downloading
            });
            if !downloading {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let result = task.await.unwrap_or_else(|error| Err(error.to_string()));
        cx.update(|cx| match result {
            Ok((path, digest)) => {
                tracing::info!(path = %path.display(), "update downloaded and verified");
                cx.global_mut::<Updater>().package = Some((path, digest));
                set_status(Status::Ready, cx);
                announce(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "update download failed");
                set_status(Status::Error(error.into()), cx);
            }
        });
    })
    .detach();
}

/// Installs the verified package; polkit asks for a password.
pub fn install(cx: &mut App) {
    let updater = cx.global::<Updater>();
    let (Status::Ready, Some((package, digest))) = (&updater.status, updater.package.clone())
    else {
        return;
    };
    let version = updater.version();
    set_status(Status::Installing, cx);
    toast(cx);
    let task = Runtime::handle(cx).spawn_blocking(move || {
        // The cached file may have changed since it was checked, including
        // one kept from an earlier check of the same release. A changed one
        // goes, so the next check downloads it again.
        if let Err(error) = release::verify(&package, &digest) {
            let _ = std::fs::remove_file(&package);
            return Err((error, true));
        }
        install::install_deb(&package).map_err(|error| (error, false))?;
        // The prompt can succeed while apt changed nothing.
        if let (Some(installed), Some(version)) = (install::installed_version(), version) {
            let installed = installed.split('-').next().unwrap_or_default();
            if semver::Version::parse(installed).ok() != Some(version.clone()) {
                return Err((
                    format!("Buzz {version} didn't install; {installed} is still installed."),
                    false,
                ));
            }
        }
        Ok(())
    });
    cx.spawn(async move |cx| {
        let result = task
            .await
            .unwrap_or_else(|error| Err((error.to_string(), false)));
        cx.update(|cx| {
            match result {
                Ok(()) => set_status(Status::Installed, cx),
                Err((error, changed)) => {
                    if changed {
                        cx.global_mut::<Updater>().package = None;
                    }
                    // Ready again, so Update now can try once more.
                    tracing::warn!(%error, "update install failed");
                    set_status(Status::Error(error.into()), cx);
                }
            }
            toast(cx);
        });
    })
    .detach();
}

/// Quits, and starts the newly installed copy once this one is gone.
pub fn restart(cx: &mut App) {
    if cx.global::<Updater>().status != Status::Installed {
        return;
    }
    #[cfg(target_os = "linux")]
    let program = match cx.global::<Updater>().kind {
        InstallKind::Deb if !cfg!(debug_assertions) => install::DEB_BINARY.to_string(),
        _ => std::env::current_exe()
            .map(|exe| {
                exe.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_string()
            })
            .unwrap_or_else(|_| install::DEB_BINARY.to_string()),
    };
    #[cfg(not(target_os = "linux"))]
    let program = std::env::current_exe()
        .map(|exe| exe.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::main_window::restart(program, cx);
}

/// Sends the browser to this platform's download.
pub fn open_download(cx: &mut App) {
    if let Some(url) = Updater::get(cx).download_url() {
        cx.open_url(&url);
    }
}

pub fn open_release(cx: &mut App) {
    if let Some(release) = Updater::get(cx).release() {
        let url = release.html_url.clone();
        cx.open_url(&url);
    }
}

/// Raises the toast the first time a version is on offer.
fn announce(cx: &mut App) {
    let updater = cx.global_mut::<Updater>();
    let version = updater.version();
    let offered = match updater.kind {
        InstallKind::Deb => updater.status == Status::Ready,
        InstallKind::Browser | InstallKind::Dev => updater.status == Status::Available,
    };
    if !offered || version.is_none() || updater.announced == version {
        return;
    }
    updater.announced = version;
    toast(cx);
}

/// The toast for the current state (buzz-app's `UpdateNotice`).
fn toast(cx: &mut App) {
    let updater = Updater::get(cx);
    let Some(version) = updater.version() else {
        return;
    };
    let whats_new = || {
        Action::new("What's new", |window, cx| {
            window.dispatch_action(Box::new(OpenUpdateSettings), cx)
        })
    };
    let notice = |title: &str, description: String, tone: ToastTone| Notice {
        key: TOAST.into(),
        title: title.to_string().into(),
        description: Some(description.into()),
        tone,
        timeout: None,
        dismissible: true,
        actions: Vec::new(),
    };
    let notice = match (&updater.status, updater.kind) {
        (Status::Ready, _) => notice(
            "Ready to update!",
            format!("Buzz {version} is downloaded. Installing asks for your password."),
            ToastTone::Info,
        )
        .action(Action::new("Update now", |_, cx| install(cx)))
        .action(whats_new()),
        (Status::Installing, _) => Notice {
            dismissible: false,
            ..notice(
                "Updating",
                format!("Installing Buzz {version}…"),
                ToastTone::Info,
            )
        },
        (Status::Installed, _) => notice(
            "Update installed",
            format!("Restart Buzz to use version {version}."),
            ToastTone::Success,
        )
        .action(Action::new("Restart Buzz", |_, cx| restart(cx))),
        (Status::Error(message), InstallKind::Deb) => {
            notice("Update failed", message.to_string(), ToastTone::Error)
                .action(Action::new("Retry", |_, cx| check(true, cx)))
        }
        (Status::Available, InstallKind::Browser) => notice(
            "Update available",
            format!("Buzz {version} is available."),
            ToastTone::Info,
        )
        .action(Action::new("Download", |_, cx| open_download(cx)))
        .action(whats_new()),
        (Status::Available, InstallKind::Dev) => notice(
            "Update available",
            format!("Buzz {version} is out. This build can't update itself."),
            ToastTone::Info,
        )
        .action(whats_new()),
        _ => {
            toasts::dismiss(TOAST, cx);
            return;
        }
    };
    toasts::show(notice, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn automatic_checks_default_on() {
        assert!(Preferences::default().automatic);
        let parsed: Preferences = serde_json::from_str("{}").unwrap();
        assert!(parsed.automatic);
        let parsed: Preferences = serde_json::from_str(r#"{"automatic":false}"#).unwrap();
        assert!(!parsed.automatic);
    }

    #[test]
    fn downloads_only_over_https_outside_development() {
        assert!(allowed_url("https://github.com/x"));
        assert_eq!(allowed_url("http://127.0.0.1/x"), cfg!(debug_assertions));
        assert!(!allowed_url("file:///etc/passwd"));
    }
}
