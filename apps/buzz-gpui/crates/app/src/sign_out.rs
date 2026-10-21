//! Sign out of Buzz, ported from buzz-app's `src-tauri/src/sign_out.rs`
//! until the fork's native library carries it. Signing out only records
//! intent in a marker outside app data, closes signing, stops agent
//! processes and restarts. The next launch, holding alone the lock for its key (and, for
//! an erase, the lock every Buzz shares), does every deletion before any
//! window, service or identity read: agent keys (when asked), the erase,
//! then the human key. Each step is safe to repeat; any failure keeps the
//! marker and Buzz exits, so the next launch retries.
//!
//! The locks, the marker's key-store name and the finished record are the
//! ones buzz-app uses, so neither app signs the shared key out while the
//! other is running with it. An erase never follows a link: every folder
//! it moves, opens or removes must be a real folder or absent, and folders
//! are moved to a trash name first so a failed key removal puts them back.

use crate::community::{AppState, identity};
use gpui_kit::{App, AppContext, AsyncApp, Task};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const FAILED: &str = "Sign out of Buzz didn't finish. Open Buzz again to retry.";
const BUSY: &str = "Another Buzz window using this sign-in is still open. Quit it, then open Buzz again to finish signing out.";
const BUSY_ALL: &str = "Another Buzz window is still open. Quit every Buzz window, then open Buzz again to finish erasing.";
const OTHERS: &str =
    "Quit every other Buzz window using this sign-in, then sign out again. Nothing was removed.";
const OTHERS_ALL: &str =
    "Quit every other Buzz window, then sign out and erase again. Nothing was removed.";
const SIGNING_OUT: &str = "Buzz is signing out in another window. Open Buzz again in a moment.";
const NOT_PREPARED: &str = "Couldn't prepare sign out; nothing was removed. Try again.";
const FENCED: &str = "Buzz had to quit to sign out safely. Open Buzz again to continue.";
const AGENT_STOPPING: &str = "Erasing didn't finish because an agent is still stopping; nothing was removed. Open Buzz again to retry.";
const ALREADY: &str = "Buzz is already signing out.";
const LINKED: &str = "Erasing is unavailable because a Buzz storage folder is a link or couldn't be checked; nothing was removed";
/// Development builds use the installed app's folders and agent keys.
const DEV_WIPE: &str = "Erasing is unavailable in development builds because they share agent keys and storage with the installed Buzz";
const NO_STORAGE: &str = "Couldn't find this app's local storage; nothing was removed";
/// This app's folder name under the data, local data and cache folders.
const IDENTIFIER: &str = "buzz-gpui";
/// Held shared by every Buzz and alone only to erase; buzz-app's name.
const LOCK: &str = ".dev.local.buzz.foundation.instance.lock";
/// How long a launch waits for an exiting or signing-out instance to let go.
const WAIT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};

/// A kept entry under the data folder. `inside` lists what stays within
/// it; `None` keeps all of it.
struct Keep {
    name: &'static str,
    inside: Option<&'static [Keep]>,
}

/// The host's folders of the Agents list, of that list before Agents2
/// took the Agents name, and of the earlier agent system, whose agents
/// Agents can still bring over.
const AGENTS: &str = buzz_host::AGENTS;
const LEGACY_AGENTS: &str = buzz_host::LEGACY_AGENTS;
const OLD_AGENTS: &str = "agent-controller";

/// What kept agents need to be identified and start again (buzz-app's
/// `KEPT`): the host's Agents list, the earlier system's list and defaults
/// to bring over from, and, with file credentials, the agents' keys.
/// Everything else under the host (hosting locks, saved logins, logs, run
/// folders) goes.
const KEEP: &[Keep] = &[
    Keep {
        name: "host",
        inside: Some(&[
            Keep {
                name: AGENTS,
                inside: Some(&[Keep {
                    name: "identities.json",
                    inside: None,
                }]),
            },
            Keep {
                name: LEGACY_AGENTS,
                inside: Some(&[Keep {
                    name: "identities.json",
                    inside: None,
                }]),
            },
            Keep {
                name: OLD_AGENTS,
                inside: Some(&[
                    Keep {
                        name: "agents.json",
                        inside: None,
                    },
                    Keep {
                        name: "defaults.json",
                        inside: None,
                    },
                ]),
            },
        ]),
    },
    Keep {
        name: "agent-keys",
        inside: None,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Choices {
    pub wipe: bool,
    pub remove_agents: bool,
}

/// The marker, the locks and the folders an erase clears.
pub(crate) struct Paths {
    marker: PathBuf,
    /// Grows by one byte each time a sign-out of this key store finishes.
    /// Shared with buzz-app, which signs the same key out.
    finished: PathBuf,
    lock: PathBuf,
    /// Held shared by every Buzz using this human key store, alone to sign
    /// out of it.
    key_lock: PathBuf,
    /// Agent ownership locks; an erase waits until none is held.
    ownership: PathBuf,
    app_data: PathBuf,
    /// Other app-owned folders: local data and caches.
    others: Vec<PathBuf>,
    /// An erase's last step: forgets remote hosts and their tokens, which
    /// live outside these folders.
    forget_hosts: fn() -> Result<(), String>,
}

/// One marker per app and key store.
fn marker_name(identifier: &str, service: &str) -> String {
    format!(".{identifier}--{service}.sign-out-pending")
}
fn finished_name(service: &str) -> String {
    format!(".{service}.sign-outs-finished")
}
/// Named for the key store alone: every app using the key shares it.
fn key_lock_name(service: &str) -> String {
    format!(".{service}.instance.lock")
}
/// How many sign-outs of this key store have finished; absent is none.
fn finished(path: &Path) -> std::io::Result<u64> {
    match fs::metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        other => other.map(|meta| meta.len()),
    }
}

impl Paths {
    pub(crate) fn resolve() -> Option<Self> {
        let data = dirs::data_dir()?;
        Some(Self {
            forget_hosts: crate::hosts::forget_remote_hosts,
            ..Self::under(
                &data,
                vec![
                    dirs::data_local_dir()?.join(IDENTIFIER),
                    dirs::cache_dir()?.join(IDENTIFIER),
                ],
            )
        })
    }

    fn under(data: &Path, mut others: Vec<PathBuf>) -> Self {
        let service = buzz_identity::SERVICE;
        let app_data = data.join(IDENTIFIER);
        others.retain(|path| *path != app_data);
        others.dedup();
        Self {
            marker: data.join(marker_name(IDENTIFIER, service)),
            finished: data.join(finished_name(service)),
            lock: data.join(LOCK),
            key_lock: data.join(key_lock_name(service)),
            ownership: app_data.join("host").join("agent-ownership"),
            app_data,
            others,
            forget_hosts: || Ok(()),
        }
    }

    /// The embedded host's folder, which holds its agent lists.
    fn host(&self) -> PathBuf {
        self.app_data.join("host")
    }

    /// Each erased folder, and what it keeps unless agents go too.
    fn targets(&self, choices: Choices) -> Vec<(&Path, Option<&'static [Keep]>)> {
        let kept = (!choices.remove_agents).then_some(KEEP);
        std::iter::once((self.app_data.as_path(), kept))
            .chain(self.others.iter().map(|path| (path.as_path(), None)))
            .collect()
    }

    /// An erase never deletes through a link: every folder it moves, opens
    /// or removes, its trash and the kept folders must be real or absent.
    fn unlinked(&self, choices: Choices) -> std::io::Result<()> {
        if choices.remove_agents {
            real_dir(&self.host())?;
            for registry in [AGENTS, LEGACY_AGENTS, OLD_AGENTS] {
                real_dir(&self.host().join(registry))?;
            }
        }
        if !choices.wipe {
            return Ok(());
        }
        for (path, kept) in self.targets(choices) {
            for path in [path.to_owned(), trash(path)] {
                if real_dir(&path)?
                    && let Some(kept) = kept
                {
                    kept_unlinked(&path, kept)?;
                }
            }
        }
        Ok(())
    }
}

/// Every kept folder under `path` is real or absent.
fn kept_unlinked(path: &Path, kept: &[Keep]) -> std::io::Result<()> {
    for keep in kept {
        if let Some(inside) = keep.inside {
            let child = path.join(keep.name);
            if real_dir(&child)? {
                kept_unlinked(&child, inside)?;
            }
        }
    }
    Ok(())
}

/// Whether `path` is a real folder; absent is `false`. A link, anything
/// else or failing to look is an error, never absence.
fn real_dir(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is a link or not a folder",
            path.display()
        ))),
    }
}

/// Every running Buzz holds both locks shared. Signing out needs this
/// key's lock alone, so no copy using the key runs; an erase also needs
/// the all-Buzz lock alone. Locks are taken key first, then all-Buzz, and
/// released in reverse, never waiting while holding one alone. `started`
/// admits one sign-out per instance: after it commits, Buzz restarts or
/// exits. `seen` is the finished record at launch: a change means a
/// sign-out finished while this instance let go of its locks.
pub(crate) struct Instance {
    locks: Mutex<Locks>,
    marker: PathBuf,
    finished: PathBuf,
    seen: u64,
    started: AtomicBool,
}

struct Locks {
    key: File,
    all: File,
}

/// Lets go explicitly rather than by closing: a process spawned from
/// another thread holds a copy of each descriptor until it execs, and a
/// lock lasts until its last copy closes, so closing alone can leave it
/// held for a moment after this instance is gone.
impl Drop for Locks {
    fn drop(&mut self) {
        let _ = self.all.unlock();
        let _ = self.key.unlock();
    }
}

impl Instance {
    /// Admits this instance's one sign-out, holding the locks it needs
    /// alone.
    fn begin(&self, choices: Choices) -> Result<(), Failure> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(refuse(ALREADY));
        }
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = locks.key.unlock() {
            return Err(lost(error));
        }
        if locks.key.try_lock().is_err() {
            return Err(self.share(&locks, false, OTHERS));
        }
        if !choices.wipe {
            return Ok(());
        }
        if let Err(error) = locks.all.unlock() {
            return Err(lost(error));
        }
        if locks.all.try_lock().is_ok() {
            return Ok(());
        }
        if let Err(error) = locks.key.unlock() {
            return Err(lost(error));
        }
        Err(self.share(&locks, true, OTHERS_ALL))
    }

    /// Withdraws a sign-out that committed nothing, returning why.
    fn abort(&self, choices: Choices, failure: Failure) -> Failure {
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        let released = if choices.wipe {
            locks.all.unlock()
        } else {
            Ok(())
        };
        match released.and_then(|()| locks.key.unlock()) {
            Ok(()) => self.share(&locks, choices.wipe, &failure.message),
            Err(error) => lost(error),
        }
    }

    /// Shares the unlocked key lock again, and the all-Buzz lock when `all`
    /// was let go too. Another process may have committed, or even
    /// finished, a sign-out meanwhile; only with neither can this instance
    /// carry on.
    fn share(&self, locks: &Locks, all: bool, message: &str) -> Failure {
        if !wait_for(|| locks.key.try_lock_shared())
            || (all && !wait_for(|| locks.all.try_lock_shared()))
        {
            return fenced();
        }
        let changed = self
            .marker
            .try_exists()
            .and_then(|pending| Ok(pending || finished(&self.finished)? != self.seen));
        match changed {
            Ok(false) => {
                self.started.store(false, Ordering::SeqCst);
                refuse(message)
            }
            Ok(true) => fenced(),
            Err(error) => lost(error),
        }
    }
}

fn lost(error: std::io::Error) -> Failure {
    tracing::error!(%error, "instance lock");
    fenced()
}

fn wait_for<E>(mut attempt: impl FnMut() -> Result<(), E>) -> bool {
    let start = Instant::now();
    loop {
        if attempt().is_ok() {
            return true;
        }
        if start.elapsed() >= WAIT {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run at launch, before anything else. Returns this instance's lock, or
/// the message to show before exiting.
fn boot_with(
    paths: &Paths,
    mut remove_agent_keys: impl FnMut(&Path) -> Result<(), String>,
    mut remove_key: impl FnMut() -> Result<(), String>,
) -> Result<Instance, String> {
    let open = |path: &Path| {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|error| {
                tracing::error!(%error, "could not open the instance lock");
                FAILED.to_owned()
            })
    };
    // A first launch may find no user data folder yet; the locks live in it.
    if let Some(data) = paths.lock.parent() {
        fs::create_dir_all(data).map_err(|error| {
            tracing::error!(%error, "could not create the user data folder");
            FAILED.to_owned()
        })?;
    }
    let locks = Locks {
        key: open(&paths.key_lock)?,
        all: open(&paths.lock)?,
    };
    let failed = |error: std::io::Error| {
        tracing::error!(%error, "instance lock");
        FAILED.to_owned()
    };
    // Shared first, then look: a sign-out that commits after this sees our
    // locks. Every time they are taken again, look again.
    loop {
        if !wait_for(|| locks.key.try_lock_shared()) || !wait_for(|| locks.all.try_lock_shared()) {
            return Err(SIGNING_OUT.into());
        }
        let Some(wipe) = pending_wipe(&paths.marker).map_err(failed)? else {
            break;
        };
        // A restarting instance may still be exiting; wait for it to let go.
        locks.all.unlock().map_err(failed)?;
        locks.key.unlock().map_err(failed)?;
        let mut busy = BUSY;
        let mut stuck = None;
        let taken = wait_for(|| {
            busy = BUSY;
            locks.key.try_lock().map_err(drop)?;
            if wipe && locks.all.try_lock().is_err() {
                busy = BUSY_ALL;
                // Never wait holding the key lock alone: if it can't be let
                // go, stop trying (`Ok` ends the wait) and fail below.
                return match locks.key.unlock() {
                    Ok(()) => Err(()),
                    Err(error) => {
                        stuck = Some(error);
                        Ok(())
                    }
                };
            }
            Ok(())
        });
        if let Some(error) = stuck {
            return Err(failed(error));
        }
        if !taken {
            return Err(busy.into());
        }
        // Read again under exclusive ownership and run only what these
        // locks cover: another launch may have finished it or committed an
        // erase since.
        match pending(&paths.marker)? {
            // Now an erase: let go and loop to take every lock.
            Some(choices) if choices.wipe && !wipe => {}
            Some(choices) => {
                if choices.wipe {
                    agents_stopped(&paths.ownership)?;
                }
                finish(paths, choices, &mut remove_agent_keys, &mut remove_key).map_err(
                    |error| {
                        tracing::error!(%error, "sign out did not finish");
                        FAILED.to_owned()
                    },
                )?
            }
            None => {}
        }
        if wipe {
            locks.all.unlock().map_err(failed)?;
        }
        locks.key.unlock().map_err(failed)?;
    }
    migrate_agents(paths, &locks)?;
    Ok(Instance {
        locks: Mutex::new(locks),
        marker: paths.marker.clone(),
        finished: paths.finished.clone(),
        seen: finished(&paths.finished).map_err(failed)?,
        started: AtomicBool::new(false),
    })
}

/// Moves the embedded host's Agents folder from its Agents2 name
/// (buzz-app's `migrate_agents`). Another running copy may still write the
/// old folder, so the move waits for Buzz's shared instance lock alone;
/// the key lock stays shared so a sign-out cannot start meanwhile.
fn migrate_agents(paths: &Paths, locks: &Locks) -> Result<(), String> {
    let old = paths.host().join(LEGACY_AGENTS);
    let failed = |error: std::io::Error| format!("Could not migrate agent storage: {error}");
    if !real_dir(&old).map_err(failed)? {
        return Ok(());
    }
    locks.all.unlock().map_err(failed)?;
    if !wait_for(|| locks.all.try_lock()) {
        return Err(
            "Close other Buzz copies, then reopen Buzz to migrate agent storage. \
                    Nothing was moved."
                .into(),
        );
    }
    // Another upgrading copy may have moved it while this one waited.
    buzz_host::migrate_agents(&paths.host())?;
    locks.all.unlock().map_err(failed)?;
    if !wait_for(|| locks.all.try_lock_shared()) {
        return Err(SIGNING_OUT.into());
    }
    Ok(())
}

/// An erase deletes storage agents use, so it waits until no supervisor
/// holds an agent's ownership lock. A plain sign-out removes only the human
/// key and skips this.
fn agents_stopped(ownership: &Path) -> Result<(), String> {
    let failed = |error: std::io::Error| {
        tracing::error!(%error, "agent ownership");
        FAILED.to_owned()
    };
    let entries = match fs::read_dir(ownership) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        other => other.map_err(failed)?,
    };
    for entry in entries {
        let path = entry.map_err(failed)?.path();
        if path.extension() != Some("lock".as_ref()) {
            continue;
        }
        // Closing the file releases a lock taken here.
        let file = File::open(&path).map_err(failed)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(AGENT_STOPPING.into()),
            Err(fs::TryLockError::Error(error)) => return Err(failed(error)),
        }
    }
    Ok(())
}

/// Whether a pending sign-out erases, to choose the locks; `None` when
/// none is pending. Malformed JSON counts as an erase, so every lock is
/// taken before `pending` reports it; a read error returns at once and
/// nothing is deleted.
fn pending_wipe(marker: &Path) -> std::io::Result<Option<bool>> {
    match fs::read(marker) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(raw) => Ok(Some(
            serde_json::from_slice::<Choices>(&raw).map_or(true, |choices| choices.wipe),
        )),
    }
}

fn trash(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".sign-out-trash");
    path.with_file_name(name)
}

fn missing(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Moves `path` to its trash name, then returns its kept folders. Resumes
/// an earlier attempt that stopped part-way.
fn move_aside(path: &Path, kept: Option<&[Keep]>) -> std::io::Result<()> {
    let moved = trash(path);
    if !real_dir(&moved)? {
        real_dir(path)?;
        missing(fs::rename(path, &moved))?;
    }
    for keep in kept.unwrap_or_default() {
        let (from, to) = (moved.join(keep.name), path.join(keep.name));
        if real_dir(&from)? && !(real_dir(path)? && real_dir(&to)?) {
            fs::create_dir_all(path)?;
            fs::rename(from, to)?;
        }
    }
    Ok(())
}

fn move_back(path: &Path, kept: Option<&[Keep]>) -> std::io::Result<()> {
    let moved = trash(path);
    if !real_dir(&moved)? {
        return Ok(());
    }
    for keep in kept.unwrap_or_default() {
        let (here, back) = (path.join(keep.name), moved.join(keep.name));
        if real_dir(path)? && real_dir(&here)? {
            real_dir(&back)?;
            fs::rename(here, back)?;
        }
    }
    // Only kept folders were returned, so anything else here is unexpected;
    // keep it.
    if real_dir(path)? {
        missing(fs::remove_dir(path))?;
    }
    fs::rename(moved, path)
}

/// Deletes `path` except what `kept` names, whether it is the original or
/// was recreated.
fn clear(path: &Path, kept: Option<&[Keep]>) -> std::io::Result<()> {
    match kept {
        None => remove_real_dir(path),
        Some(kept) => clear_except(path, kept),
    }
}

fn clear_except(path: &Path, kept: &[Keep]) -> std::io::Result<()> {
    if !real_dir(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        // `file_type` does not follow links: a link is removed or kept
        // itself, never entered.
        let kind = entry.file_type()?;
        match kept.iter().find(|keep| entry.file_name() == keep.name) {
            Some(Keep {
                inside: Some(inside),
                ..
            }) if kind.is_dir() => clear_except(&entry.path(), inside)?,
            Some(_) => {}
            None if kind.is_dir() => fs::remove_dir_all(entry.path())?,
            None => fs::remove_file(entry.path())?,
        }
    }
    Ok(())
}

/// Deletes a real folder; absent is done, and a link is refused, never
/// followed or unlinked.
fn remove_real_dir(path: &Path) -> std::io::Result<()> {
    if !real_dir(path)? {
        return Ok(());
    }
    missing(fs::remove_dir_all(path))
}

/// Deletes `path`'s trash, refusing a link at the trash or a kept folder.
fn remove_trash(path: &Path, kept: Option<&[Keep]>) -> std::io::Result<()> {
    let moved = trash(path);
    if kept.is_some() && real_dir(&moved)? {
        for keep in kept.unwrap_or_default() {
            real_dir(&moved.join(keep.name))?;
        }
    }
    remove_real_dir(&moved)
}

/// The pending sign-out's choices, if any. Unreadable or malformed is the
/// message shown before exiting, never absence.
fn pending(marker: &Path) -> Result<Option<Choices>, String> {
    let raw = match fs::read(marker) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        other => other,
    };
    raw.map_err(|error| error.to_string())
        .and_then(|raw| serde_json::from_slice(&raw).map_err(|e| e.to_string()))
        .map(Some)
        .map_err(|error| {
            tracing::error!(%error, "sign out did not finish");
            FAILED.to_owned()
        })
}

fn finish(
    paths: &Paths,
    choices: Choices,
    remove_agent_keys: impl FnOnce(&Path) -> Result<(), String>,
    remove_key: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    // Agent keys go first, while their list is still in place; nothing is
    // moved until every key is gone, so a list confirmed absent means it's
    // done. Failing to look is not absence: stop before moving anything.
    paths
        .unlinked(choices)
        .map_err(|error| format!("check erased folders: {error}"))?;
    let host = paths.host();
    if choices.remove_agents
        && host
            .try_exists()
            .map_err(|error| format!("check agent list: {error}"))?
    {
        remove_agent_keys(&host).map_err(|error| format!("remove agent keys: {error}"))?;
    }
    let targets = if choices.wipe {
        paths.targets(choices)
    } else {
        Vec::new()
    };
    let mut moved = Vec::new();
    let mut outcome = Ok(());
    for &(path, kept) in &targets {
        moved.push((path, kept));
        if let Err(error) = move_aside(path, kept) {
            outcome = Err(format!("move {} aside: {error}", path.display()));
            break;
        }
    }
    if outcome.is_ok() {
        // Idempotent: an absent key is success, so a resumed attempt passes.
        outcome = remove_key();
    }
    if let Err(error) = outcome {
        // Partly moved folders go back; the marker stays so the next launch
        // retries.
        for &(path, kept) in moved.iter().rev() {
            if let Err(undo) = move_back(path, kept) {
                tracing::error!(path = %path.display(), %undo, "could not restore");
            }
        }
        return Err(error);
    }
    // The key is gone, so nothing is rolled back from here: clear what was
    // moved aside and anything in place now, then the marker.
    for (path, kept) in targets {
        remove_trash(path, kept)
            .and_then(|()| clear(path, kept))
            .map_err(|error| format!("delete erased {}: {error}", path.display()))?;
    }
    if choices.wipe {
        (paths.forget_hosts)().map_err(|error| format!("forget remote hosts: {error}"))?;
    }
    // Recorded before the marker goes; a resumed attempt records it again.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.finished)
        .and_then(|mut record| std::io::Write::write_all(&mut record, b"."))
        .map_err(|error| format!("record finished: {error}"))?;
    fs::remove_file(&paths.marker).map_err(|error| format!("remove marker: {error}"))
}

fn write_marker(path: &Path, choices: Choices) -> std::io::Result<()> {
    let staged = path.with_extension("staged");
    fs::write(&staged, serde_json::to_vec(&choices)?)?;
    fs::rename(&staged, path)
}

/// A refusal the dialog shows. `fenced` means this instance can't continue
/// (its locks or agents are in an unknown state): Buzz exits instead, and
/// it never reaches the dialog.
#[derive(Debug, PartialEq)]
struct Failure {
    message: String,
    fenced: bool,
}

fn refuse(message: &str) -> Failure {
    Failure {
        message: message.into(),
        fenced: false,
    }
}

fn fenced() -> Failure {
    Failure {
        message: FENCED.into(),
        fenced: true,
    }
}

/// Admits the sign-out and commits the marker. A refusal before the
/// marker withdraws and gives the locks back.
fn commit(instance: &Instance, paths: &Paths, choices: Choices) -> Result<(), Failure> {
    instance.begin(choices)?;
    let prepared = paths
        .unlinked(choices)
        .map_err(|error| {
            tracing::error!(%error, "sign out refused");
            refuse(LINKED)
        })
        .and_then(|()| {
            write_marker(&paths.marker, choices).map_err(|error| {
                tracing::error!(%error, "could not write the sign-out marker");
                refuse(NOT_PREPARED)
            })
        });
    prepared.map_err(|failure| {
        if failure.fenced {
            failure
        } else {
            instance.abort(choices, failure)
        }
    })
}

/// Why this build can't sign out with these choices, if it can't.
fn refusal(debug: bool, wipe: bool, remove_agents: bool) -> Option<&'static str> {
    if remove_agents && !wipe {
        Some("Removing agents is part of erasing this device")
    } else if wipe && debug {
        Some(DEV_WIPE)
    } else {
        None
    }
}

fn current_refusal(wipe: bool, remove_agents: bool) -> Option<&'static str> {
    refusal(cfg!(debug_assertions), wipe, remove_agents)
}

/// Why erasing is unavailable here, if it is, so the dialog can say so up
/// front; [`sign_out`] still enforces it.
pub fn wipe_refusal() -> Option<&'static str> {
    current_refusal(true, false)
}

static INSTANCE: OnceLock<Instance> = OnceLock::new();

/// Call once at the top of `main`, before any window, service or identity
/// read: finishes a pending sign-out and takes this instance's locks. If
/// it can't, Buzz explains and exits without opening.
pub fn boot() {
    let Some(paths) = Paths::resolve() else {
        tracing::warn!("no data folder; sign out is unavailable");
        return;
    };
    let app_data = paths.app_data.clone();
    match boot_with(
        &paths,
        |host| remove_agent_keys(&app_data, host),
        identity::remove,
    ) {
        Ok(instance) => {
            let _ = INSTANCE.set(instance);
        }
        Err(message) => exit_with(&message),
    }
}

/// "Also remove my agents": deletes every local agent's key, from the
/// store the embedded host uses: the earlier system's agents, then the
/// Agents list's. Absence is success, so it repeats safely.
fn remove_agent_keys(app_data: &Path, host: &Path) -> Result<(), String> {
    use buzz_host::credentials::{FileCredentials, KeyringCredentials};
    let credentials: Box<dyn buzz_host::Credentials> =
        match crate::config::Config::load().embedded.credentials {
            crate::config::CredentialsBackend::Keyring => {
                Box::new(KeyringCredentials::new(crate::hosts::AGENT_KEY_SERVICE))
            }
            crate::config::CredentialsBackend::Files => Box::new(
                FileCredentials::new(app_data.join("agent-keys"))
                    .map_err(|error| error.to_string())?,
            ),
        };
    let old = host.join(OLD_AGENTS);
    if old
        .try_exists()
        .map_err(|error| format!("check agent registry: {error}"))?
    {
        buzz_agent_controller::delete_local_agent_keys(old, &*credentials)?;
    }
    for folder in [AGENTS, LEGACY_AGENTS] {
        buzz_agent_controller::AppAgents::open(host.join(folder).join("identities.json"))
            .delete_keys(&*credentials)?;
    }
    Ok(())
}

/// Records the choices, closes signing, stops agent processes and
/// restarts; the next launch deletes. Errors are what the dialog shows.
pub fn sign_out(choices: Choices, cx: &mut App) -> Task<Result<(), String>> {
    if let Some(message) = current_refusal(choices.wipe, choices.remove_agents) {
        return Task::ready(Err(message.into()));
    }
    let (Some(paths), Some(instance)) = (Paths::resolve(), INSTANCE.get()) else {
        return Task::ready(Err(NO_STORAGE.into()));
    };
    let host = crate::hosts::Hosts::embedded(cx);
    cx.spawn(async move |cx| {
        // The locks may wait, so admission runs off the UI thread.
        let committed = cx
            .background_spawn(async move { commit(instance, &paths, choices) })
            .await;
        match committed {
            Ok(()) => {}
            Err(failure) if failure.fenced => return exit_fenced(cx).await,
            Err(failure) => return Err(failure.message),
        }
        // Committed: the key is removed at the next launch, so nothing more
        // is signed with it.
        cx.update(AppState::close_sessions);
        // A pairing attempt holds a copy of the key until it sends it.
        cx.update(crate::ui::settings::close_pairing);
        let stopped = cx
            .background_spawn(async move { host.map_or(Ok(()), |host| host.shutdown()) })
            .await;
        if let Err(error) = stopped {
            tracing::error!(%error, "agents did not stop");
            return exit_fenced(cx).await;
        }
        cx.update(|cx| cx.restart());
        Ok(())
    })
}

/// This instance can't carry on: explain, then exit. Agent supervisors
/// stop their agents when Buzz exits; a pending erase waits for them at
/// launch.
async fn exit_fenced(cx: &mut AsyncApp) -> Result<(), String> {
    cx.update(|cx| {
        AppState::close_sessions(cx);
        message_window(FENCED.into(), cx);
    });
    std::future::pending().await
}

/// Shown when [`boot`] fails: no main window opens, so nothing recreates
/// erased storage.
fn exit_with(message: &str) -> ! {
    tracing::error!(message, "sign out");
    let message = message.to_string();
    gpui_kit::application()
        .with_assets(buzz_ui::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            buzz_ui::init(cx);
            message_window(message.into(), cx);
        });
    std::process::exit(1)
}

/// A small "Buzz" window with `message` and OK; closing it exits.
fn message_window(message: gpui_kit::SharedString, cx: &mut App) {
    use buzz_ui::{ActiveTheme, Button, ButtonVariant, TypeStyled};
    use gpui_kit::*;

    struct Message(SharedString);
    impl Render for Message {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let c = *cx.colors();
            div()
                .size_full()
                .flex()
                .flex_col()
                .justify_between()
                .gap(rems(1.))
                .p(rems(1.5))
                .bg(c.surface_panel)
                .text_color(c.text_standard)
                .child(div().text_body().child(self.0.clone()))
                .child(
                    div().flex().justify_end().child(
                        Button::new("sign-out-exit", "OK")
                            .variant(ButtonVariant::Prominent)
                            .on_click(|_, _, _| std::process::exit(1)),
                    ),
                )
        }
    }

    let bounds = Bounds::centered(None, size(px(440.), px(180.)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Buzz".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            window.on_window_should_close(cx, |_, _| std::process::exit(1));
            cx.new(|_| Message(message))
        },
    );
    if let Err(error) = opened {
        tracing::error!(%error, "could not show the sign-out message");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;
