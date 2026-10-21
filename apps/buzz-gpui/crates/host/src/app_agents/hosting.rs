//! Which GUI runs each Agents2 agent, a port of the `Hosting` in buzz-app's
//! `src-tauri/src/app_agents.rs`.
//!
//! Every GUI connected to this host hears the same events for the agents it
//! lists, and so does every copy of the app whose host shares this state
//! directory (the packaged app and any dev builds). One runs each agent.
//! Within this host a connection holds the agent until it lets go or
//! closes. Across copies, a packaged host always runs what it can and holds
//! `<pubkey>.packaged` while it does. Dev builds step aside while that is
//! held, and otherwise run each agent from whichever holds `<pubkey>.lock`.
//! The OS lets go of both when a copy exits, however it exits.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

type Result<T> = std::result::Result<T, String>;

/// Who asks: a connection to this host.
pub(crate) trait Holder: Clone {
    fn same(&self, other: &Self) -> bool;
    /// False once the connection has closed, and so let go of all it held.
    fn alive(&self) -> bool;
}

pub(crate) struct Hosting<H> {
    dir: PathBuf,
    packaged: bool,
    /// Who holds each agent, and the lock file it holds for it: `.packaged`
    /// or `.lock`. A packaged host may hold an agent before its file, which
    /// a dev build checking for it holds briefly.
    held: Mutex<HashMap<String, (H, Option<File>)>>,
}

impl<H: Holder> Hosting<H> {
    /// `packaged` is false for a dev build, which steps aside for a
    /// packaged app.
    pub(crate) fn new(dir: PathBuf, packaged: bool) -> Self {
        Self {
            dir,
            packaged,
            held: Mutex::default(),
        }
    }

    fn held(&self) -> MutexGuard<'_, HashMap<String, (H, Option<File>)>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `by` runs the agent. Another connection holding it keeps it.
    /// A packaged host always runs it otherwise, so a stuck dev build can
    /// never silence it. A dev build takes it if no other copy runs it, and
    /// lets go while the packaged app does.
    pub(crate) fn claim(&self, pubkey: &str, by: &H) -> Result<bool> {
        if pubkey.len() != 64
            || !pubkey
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err("Invalid agent".into());
        }
        std::fs::create_dir_all(&self.dir).map_err(|_| "Could not open agent hosting")?;
        let packaged = self.dir.join(format!("{pubkey}.packaged"));
        let mut held = self.held();
        // A connection that closed while this claim waited has already
        // let go of everything; holding for it now would hold for good.
        if !by.alive() || held.get(pubkey).is_some_and(|(holder, _)| !holder.same(by)) {
            return Ok(false);
        }
        if self.packaged {
            let (_, file) = held
                .entry(pubkey.into())
                .or_insert_with(|| (by.clone(), None));
            if file.is_none() {
                let lock_file = open_lock(&packaged)?;
                // A dev build checking for it holds it briefly; this tries
                // again next time.
                if lock(lock_file.try_lock())? {
                    *file = Some(lock_file);
                }
            }
            return Ok(true);
        }
        if !lock(open_lock(&packaged)?.try_lock_shared())? {
            held.remove(pubkey);
            return Ok(false);
        }
        if !held.contains_key(pubkey) {
            let file = open_lock(&self.dir.join(format!("{pubkey}.lock")))?;
            if lock(file.try_lock())? {
                held.insert(pubkey.into(), (by.clone(), Some(file)));
            }
        }
        Ok(held.contains_key(pubkey))
    }

    /// Lets another connection or copy run the agent, if `by` held it.
    pub(crate) fn release(&self, pubkey: &str, by: &H) {
        let mut held = self.held();
        if held.get(pubkey).is_some_and(|(holder, _)| holder.same(by)) {
            held.remove(pubkey);
        }
    }

    /// Lets go of everything `by` held, as when its connection closes.
    pub(crate) fn release_all(&self, by: &H) {
        self.held().retain(|_, (holder, _)| !holder.same(by));
    }

    /// Lets go of a deleted agent.
    pub(crate) fn forget(&self, pubkey: &str) {
        self.held().remove(pubkey);
    }
}

/// Whether a try at a lock took it; closing the file lets go.
fn lock(attempt: std::result::Result<(), TryLockError>) -> Result<bool> {
    match attempt {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(_)) => Err("Could not read agent hosting".into()),
    }
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "Could not open agent hosting lock".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 stands for a closed connection.
    impl Holder for u32 {
        fn same(&self, other: &Self) -> bool {
            self == other
        }
        fn alive(&self) -> bool {
            *self != 0
        }
    }

    #[test]
    fn a_closed_connection_holds_nothing() {
        for packaged in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let agent = "a".repeat(64);
            let host = Hosting::new(dir.path().join("hosts"), packaged);
            assert!(!host.claim(&agent, &0).unwrap());
            assert!(host.claim(&agent, &1).unwrap());
        }
    }

    #[test]
    fn one_copy_runs_each_agent_until_it_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = ("a".repeat(64), "b".repeat(64));
        let one = Hosting::new(dir.path().join("hosts"), false);
        let other = Hosting::new(dir.path().join("hosts"), false);
        assert!(one.claim(&first, &1).unwrap());
        assert!(one.claim(&first, &1).unwrap());
        assert!(!other.claim(&first, &1).unwrap());
        assert!(other.claim(&second, &1).unwrap());
        // As when that copy quits.
        drop(one);
        assert!(other.claim(&first, &1).unwrap());
        assert!(other.claim("../x", &1).is_err());
    }

    #[test]
    fn a_packaged_app_runs_what_it_can_and_dev_builds_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, other) = ("a".repeat(64), "b".repeat(64));
        let dev = Hosting::new(dir.path().join("hosts"), false);
        let packaged = Hosting::new(dir.path().join("hosts"), true);
        assert!(dev.claim(&agent, &1).unwrap());
        assert!(dev.claim(&other, &1).unwrap());
        // The packaged app runs it at once, though the dev build had it
        // first,
        assert!(packaged.claim(&agent, &1).unwrap());
        // and the dev build lets go at its next claim.
        assert!(!dev.claim(&agent, &1).unwrap());
        assert!(packaged.claim(&agent, &1).unwrap());
        // A second dev build cannot take it either.
        let another = Hosting::new(dir.path().join("hosts"), false);
        assert!(!another.claim(&agent, &1).unwrap());
        // One the packaged app does not ask for stays with the dev build.
        assert!(dev.claim(&other, &1).unwrap());
        // As when it can no longer run it.
        packaged.release(&agent, &1);
        assert!(dev.claim(&agent, &1).unwrap());
        assert!(!another.claim(&agent, &1).unwrap());
    }

    #[test]
    fn one_connection_runs_each_agent_until_it_lets_go_or_closes() {
        for packaged in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (agent, other) = ("a".repeat(64), "b".repeat(64));
            let host = Hosting::new(dir.path().join("hosts"), packaged);
            assert!(host.claim(&agent, &1).unwrap());
            assert!(!host.claim(&agent, &2).unwrap());
            assert!(host.claim(&other, &2).unwrap());
            // Only the holder lets go.
            host.release(&agent, &2);
            assert!(!host.claim(&agent, &2).unwrap());
            host.release(&agent, &1);
            assert!(host.claim(&agent, &2).unwrap());
            // As when the connection closes.
            host.release_all(&2);
            assert!(host.claim(&other, &1).unwrap());
            assert!(host.claim(&agent, &1).unwrap());
            host.forget(&agent);
            assert!(host.claim(&agent, &2).unwrap());
        }
    }
}
