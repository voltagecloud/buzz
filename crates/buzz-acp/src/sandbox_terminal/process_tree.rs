//! Linux subreaper teardown scoped to a dedicated terminal worker.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};

const MAX_PROCESSES: usize = 4096;
const MAX_DESCENDANTS: usize = 256;
const MAX_STAT_BYTES: u64 = 4096;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct Identity {
    pid: i32,
    parent: i32,
    started: u64,
    zombie: bool,
}

/// Establish and verify containment before any shell can exist. This process
/// owns only one terminal, so adopted children can never be harness siblings.
pub(super) fn establish() -> Result<()> {
    nix::sys::prctl::set_child_subreaper(true).context("terminal subreaper setup failed")?;
    ensure!(
        nix::sys::prctl::get_child_subreaper()?,
        "terminal subreaper unavailable"
    );
    nix::sys::prctl::set_pdeathsig(Some(nix::sys::signal::Signal::SIGTERM))?;
    ensure!(
        nix::unistd::getppid().as_raw() > 1,
        "terminal parent disappeared"
    );
    // Refuse old kernels or seccomp profiles before creating a shell. pidfds
    // plus start-time verification keep PID reuse from targeting another task.
    let own = pidfd_open(rustix::process::getpid(), PidfdFlags::empty())?;
    pidfd_send_signal(&own, Signal::CONT)?;
    ensure!(
        descendants()?.is_empty(),
        "terminal worker already owns children"
    );
    Ok(())
}

fn identity(pid: i32) -> Result<Option<Identity>> {
    let file = match std::fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut text = String::new();
    file.take(MAX_STAT_BYTES + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() <= MAX_STAT_BYTES as usize,
        "terminal process metadata exceeds bound"
    );
    let (_, fields) = text
        .rsplit_once(')')
        .context("invalid terminal process metadata")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    ensure!(fields.len() >= 20, "incomplete terminal process metadata");
    Ok(Some(Identity {
        pid,
        parent: fields[1].parse()?,
        started: fields[19].parse()?,
        zombie: matches!(fields[0], "Z" | "X"),
    }))
}

fn descendants() -> Result<Vec<Identity>> {
    let mut processes = HashMap::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        scanned += 1;
        ensure!(
            scanned <= MAX_PROCESSES,
            "terminal process scan exceeds bound"
        );
        if let Some(process) = identity(pid)? {
            processes.insert(pid, process);
        }
    }
    select_descendants(processes, std::process::id() as i32)
}

fn select_descendants(processes: HashMap<i32, Identity>, own: i32) -> Result<Vec<Identity>> {
    let mut selected = HashSet::from([own]);
    loop {
        let before = selected.len();
        for process in processes.values() {
            if selected.contains(&process.parent) {
                selected.insert(process.pid);
            }
        }
        ensure!(
            selected.len() <= MAX_DESCENDANTS + 1,
            "terminal descendant limit exceeded"
        );
        if before == selected.len() {
            break;
        }
    }
    Ok(processes
        .into_values()
        .filter(|process| process.pid != own && selected.contains(&process.pid))
        .collect())
}

fn signal(process: Identity, signal: Signal) -> Result<()> {
    if process.zombie {
        return Ok(());
    }
    let pid = Pid::from_raw(process.pid).context("invalid terminal process id")?;
    let fd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !identity(process.pid)?.is_some_and(|current| current.started == process.started) {
        return Ok(());
    }
    match pidfd_send_signal(&fd, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Stop, kill and reap the complete terminal tree, including reparented
/// setsid/double-fork children. A containment failure is always an error.
pub(super) fn cleanup() -> Result<()> {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        let children = descendants()?;
        for child in &children {
            signal(*child, Signal::STOP)?;
        }
        for child in children {
            signal(child, Signal::KILL)?;
        }
        for _ in 0..=MAX_DESCENDANTS {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if descendants()?.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("terminal process teardown timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_process_selection_excludes_worker_and_siblings() {
        let processes = [(10, 1), (20, 10), (30, 20), (40, 1), (50, 10)]
            .into_iter()
            .map(|(pid, parent)| {
                (
                    pid,
                    Identity {
                        pid,
                        parent,
                        started: 1,
                        zombie: false,
                    },
                )
            })
            .collect();
        let mut selected: Vec<_> = select_descendants(processes, 10)
            .unwrap()
            .into_iter()
            .map(|process| process.pid)
            .collect();
        selected.sort();
        assert_eq!(selected, [20, 30, 50]);
    }

    #[test]
    fn terminal_process_selection_refuses_excess_descendants() {
        let processes = (20..20 + MAX_DESCENDANTS as i32 + 1)
            .map(|pid| {
                (
                    pid,
                    Identity {
                        pid,
                        parent: 10,
                        started: 1,
                        zombie: false,
                    },
                )
            })
            .collect();
        assert!(select_descendants(processes, 10).is_err());
    }
}
