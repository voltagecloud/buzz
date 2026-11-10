use super::{
    format::{self, ArchivePlan as Plan, Kind},
    Error, RestoreBudget, Restored,
};
use buzz_sdk::sandbox_attach::snapshot::{MAX_FILES, MAX_SOURCE_BYTES};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

fn invalid(message: &'static str) -> Error {
    Error::Invalid(message)
}
fn cancelled(token: &CancellationToken) -> Result<(), Error> {
    if token.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

async fn git(
    directory: &Path,
    args: &[&str],
    input: Option<Vec<u8>>,
    cap: usize,
    timeout: Duration,
    token: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    cancelled(token)?;
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.logAllRefUpdates=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "protocol.allow=never",
        ])
        .arg("-C")
        .arg(directory)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().map_err(|_| {
        Error::Git("Git unavailable or denied; enable Paseo host tools for snapshots")
    })?;
    let id = child
        .id()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or(Error::Git("missing Git process identity"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(Error::Git("missing Git output"))?;
    let stdin = child.stdin.take();
    let operation = async {
        let writer = async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let reader = async {
            let mut bytes = Vec::new();
            stdout.take(cap as u64 + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (_, bytes) = tokio::try_join!(writer, reader)?;
        if bytes.len() > cap {
            return Err(invalid("Git output exceeds snapshot budget"));
        }
        let pid =
            rustix::process::Pid::from_raw(id).ok_or(Error::Git("invalid Git process identity"))?;
        loop {
            if let Some(status) = rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            )
            .map_err(std::io::Error::from)?
            {
                if status.exit_status() != Some(0) {
                    return Err(Error::Git("fixed Git import/inventory operation refused"));
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(bytes)
    };
    let result = tokio::select! {result=operation=>result,_=token.cancelled()=>Err(Error::Cancelled),_=tokio::time::sleep(timeout)=>Err(Error::Git("Git operation timed out"))};
    {
        // The leader remains waitable (WNOWAIT), fencing PID/group reuse during teardown.
        match killpg(Pid::from_raw(id), Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(_) => return Err(Error::Git("Git process-tree stop failed")),
        }
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .map_err(|_| Error::Git("Git process-tree cleanup timed out"))??;
    }
    result
}

pub(super) async fn check_git() -> Result<(), Error> {
    let current = std::env::current_dir()?;
    let output = git(
        &current,
        &["--version"],
        None,
        4096,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await?;
    let version = std::str::from_utf8(&output)
        .ok()
        .and_then(|s| s.strip_prefix("git version "))
        .and_then(|s| s.split_whitespace().next())
        .ok_or(Error::Git("Git version probe refused"))?;
    let mut parts = version.split('.');
    let major = parts.next().and_then(|s| s.parse::<u32>().ok());
    let minor = parts.next().and_then(|s| s.parse::<u32>().ok());
    if !matches!((major,minor),(Some(major),Some(minor))if major>2||(major==2&&minor>=29)) {
        return Err(Error::Git("snapshot extraction requires Git 2.29 or newer"));
    }
    Ok(())
}

struct Stage {
    directory: tempfile::TempDir,
    ready: tempfile::TempDir,
    plan: Plan,
    block: u64,
    _lock: Flock<std::fs::File>,
}
fn usage(root: &Path, block: u64, token: &CancellationToken) -> Result<u64, Error> {
    let mut pending = vec![root.to_path_buf()];
    let mut total = 0u64;
    let mut count = 0;
    while let Some(path) = pending.pop() {
        cancelled(token)?;
        count += 1;
        if count > MAX_FILES * 5 {
            return Err(invalid("snapshot store entry budget exceeded"));
        }
        let meta = fs::symlink_metadata(&path)?;
        total = total
            .checked_add(
                meta.len()
                    .max(meta.blocks().saturating_mul(512))
                    .saturating_add(block),
            )
            .ok_or_else(|| invalid("snapshot store usage overflow"))?;
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                if pending.len() + count >= MAX_FILES * 5 {
                    return Err(invalid("snapshot directory entry budget exceeded"));
                }
                pending.push(entry?.path());
            }
        }
    }
    Ok(total)
}
fn projection(plan: &Plan, block: u64) -> Result<u64, Error> {
    let mut bytes = plan.bytes;
    let mut directories = HashSet::new();
    let mut ref_directories = HashSet::new();
    let mut nodes = plan.files.len() as u64;
    for stats in plan.inventory.values() {
        bytes = bytes
            .checked_add(stats.objects as u64 * 64)
            .ok_or_else(|| invalid("snapshot index estimate overflow"))?;
        nodes += 32 + stats.refs.len() as u64;
    }
    // Borrow prefixes instead of allocating a copy for every ancestor.
    for path in plan.files.keys().map(PathBuf::as_path).chain(
        plan.manifest
            .repositories
            .iter()
            .map(|r| Path::new(&r.path)),
    ) {
        for parent in path.ancestors() {
            directories.insert(parent);
            if directories.len() as u64 + nodes > MAX_FILES as u64 * 5 {
                return Err(invalid("snapshot directory budget exceeded"));
            }
        }
    }
    for (index, stats) in &plan.inventory {
        for (_, name) in &stats.refs {
            for parent in Path::new(name).ancestors().skip(1) {
                ref_directories.insert((*index, parent));
                if (directories.len() + ref_directories.len()) as u64 + nodes > MAX_FILES as u64 * 5
                {
                    return Err(invalid("snapshot directory budget exceeded"));
                }
            }
        }
    }
    for item in &plan.items {
        if item.path.ends_with("/history.bundle") {
            bytes = bytes
                .checked_add(item.size)
                .ok_or_else(|| invalid("snapshot pack estimate overflow"))?;
        }
    }
    nodes += (directories.len() + ref_directories.len()) as u64;
    // Git index paths/entries, transient metadata and operational reserve.
    bytes
        .checked_add(nodes.saturating_mul(block))
        .and_then(|n| n.checked_add(24 * 1024 * 1024))
        .ok_or_else(|| invalid("snapshot capacity estimate overflow"))
}
fn cleanup(root: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(root) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0usize;
    while let Some(path) = pending.pop() {
        count += 1;
        if count > MAX_FILES * 5 {
            return Err(std::io::Error::other(
                "snapshot cleanup entry budget exceeded",
            ));
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            for entry in fs::read_dir(&path)? {
                if pending.len() + count >= MAX_FILES * 5 {
                    return Err(std::io::Error::other(
                        "snapshot cleanup entry budget exceeded",
                    ));
                }
                pending.push(entry?.path());
            }
        }
    }
    fs::remove_dir_all(root)
}
fn prepare(
    bytes: Vec<u8>,
    destination: PathBuf,
    store: PathBuf,
    budget: RestoreBudget,
    token: CancellationToken,
) -> Result<Stage, Error> {
    cancelled(&token)?;
    if budget.work_bytes == 0
        || budget.store_bytes == 0
        || budget.work_bytes > 2 * 1024 * 1024 * 1024
        || budget.store_bytes > 2 * 1024 * 1024 * 1024
    {
        return Err(invalid("invalid snapshot work/store budget"));
    }
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(store.join(".buzz-snapshot.lock"))?;
    if !lock_file.metadata()?.is_file() {
        return Err(invalid("snapshot store lock is not regular"));
    }
    let lock = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock)
        .map_err(|_| invalid("snapshot store is busy"))?;
    let plan = format::inspect_archive(&bytes, &token)?;
    let info = rustix::fs::statvfs(&store).map_err(std::io::Error::from)?;
    let block = info.f_frsize.max(512);
    let needed = projection(&plan, block)?;
    let available = info.f_bavail.saturating_mul(info.f_frsize);
    if needed > budget.work_bytes
        || needed > available
        || usage(&store, block, &token)?.saturating_add(needed) > budget.store_bytes
    {
        return Err(invalid(
            "snapshot staging/store capacity exceeds configured budget",
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("missing snapshot parent"))?;
    let directory = tempfile::Builder::new()
        .prefix(".buzz-snapshot-work-")
        .tempdir_in(parent)?;
    let ready = tempfile::Builder::new()
        .prefix(".buzz-snapshot-ready-")
        .tempdir_in(parent)?;
    let mut roots = HashSet::new();
    for repo in &plan.manifest.repositories {
        let path = ready.path().join(&repo.path);
        fs::create_dir_all(&path)?;
        let meta = fs::metadata(path)?;
        if !roots.insert((meta.dev(), meta.ino())) {
            return Err(invalid(
                "snapshot repository paths alias on this filesystem",
            ));
        }
    }
    let reversed: HashMap<_, _> = plan
        .files
        .iter()
        .map(|(path, name)| (name.as_str(), path))
        .collect();
    let result = format::walk(&bytes, &token, |item, reader| {
        cancelled(&token)?;
        if item.kind != Kind::File {
            return Ok(());
        }
        let target = if let Some(path) = reversed.get(item.path.as_str()) {
            Some(ready.path().join(path))
        } else if item.path.ends_with("/history.bundle") {
            Some(directory.path().join(&item.path))
        } else {
            None
        };
        if let Some(target) = target {
            let parent = target
                .parent()
                .ok_or_else(|| invalid("missing snapshot file parent"))?;
            fs::create_dir_all(parent)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)?;
            std::io::copy(reader, &mut file)?;
            file.flush()?;
            if reversed.contains_key(item.path.as_str()) {
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(if item.mode & 0o111 != 0 { 0o500 } else { 0o400 }),
                )?;
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        cleanup(directory.path()).map_err(Error::Cleanup)?;
        cleanup(ready.path()).map_err(Error::Cleanup)?;
        return Err(error);
    }
    Ok(Stage {
        directory,
        ready,
        plan,
        block,
        _lock: lock,
    })
}

async fn import(stage: &Stage, token: &CancellationToken) -> Result<(), Error> {
    let mut tracked = 0usize;
    for (index, repo) in stage.plan.manifest.repositories.iter().enumerate() {
        cancelled(token)?;
        let root = stage.ready.path().join(&repo.path);
        let bundle = stage
            .directory
            .path()
            .join(format!("snapshot/repositories/{index}/history.bundle"));
        let stats = stage
            .plan
            .inventory
            .get(&index)
            .ok_or_else(|| invalid("missing snapshot inventory"))?;
        if stats
            .refs
            .iter()
            .any(|(oid, _)| oid.bytes().all(|b| b == b'0'))
        {
            return Err(invalid("null snapshot reference object"));
        }
        let object_format = if repo.head.len() == 64 {
            "--object-format=sha256"
        } else {
            "--object-format=sha1"
        };
        git(
            &root,
            &["init", "--template=", object_format],
            None,
            4096,
            Duration::from_secs(60),
            token,
        )
        .await?;
        let bundle = bundle
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 local bundle path"))?;
        git(
            &root,
            &["bundle", "unbundle", bundle],
            None,
            8 * 1024 * 1024,
            Duration::from_secs(60),
            token,
        )
        .await?;

        let mut updates = Vec::new();
        for (oid, name) in &stats.refs {
            if name != "HEAD" {
                updates.extend_from_slice(format!("create {name}\0{oid}\0").as_bytes());
            }
        }
        git(
            &root,
            &["update-ref", "--stdin", "-z"],
            Some(updates),
            4096,
            Duration::from_secs(60),
            token,
        )
        .await?;
        git(
            &root,
            &["update-ref", "--no-deref", "HEAD", &repo.head],
            None,
            4096,
            Duration::from_secs(60),
            token,
        )
        .await?;
        if !repo.shallow.is_empty() {
            fs::write(
                root.join(".git/shallow"),
                format!("{}\n", repo.shallow.join("\n")),
            )?;
        }
        git(
            &root,
            &[
                "fsck",
                "--full",
                "--strict",
                "--no-reflogs",
                "--no-dangling",
            ],
            None,
            1024 * 1024,
            Duration::from_secs(60),
            token,
        )
        .await?;
        let objects = git(
            &root,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectsize)",
            ],
            None,
            4 * 1024 * 1024,
            Duration::from_secs(60),
            token,
        )
        .await?;
        let sizes: Vec<u64> = std::str::from_utf8(&objects)
            .map_err(|_| invalid("invalid imported object inventory"))?
            .lines()
            .map(|s| {
                s.parse()
                    .map_err(|_| invalid("invalid imported object size"))
            })
            .collect::<Result<_, _>>()?;
        let expanded = sizes.iter().try_fold(0u64, |sum, n| {
            sum.checked_add(*n)
                .ok_or_else(|| invalid("imported object size overflow"))
        })?;
        if sizes.len() != stats.objects as usize
            || expanded != stats.expanded_bytes
            || expanded > MAX_SOURCE_BYTES
        {
            return Err(invalid("imported objects differ from bounded preflight"));
        }
        let paths = git(
            &root,
            &["ls-tree", "-rz", "--name-only", "HEAD"],
            None,
            8 * 1024 * 1024,
            Duration::from_secs(60),
            token,
        )
        .await?;
        let paths: Vec<_> = paths.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
        tracked += paths.len();
        if tracked > MAX_FILES {
            return Err(invalid("snapshot tracked-path budget exceeded"));
        }
        let mut skipped = Vec::new();
        for raw in paths {
            let path =
                std::str::from_utf8(raw).map_err(|_| invalid("non-UTF8 tracked snapshot path"))?;
            if format::excluded(&format::safe(path)?) {
                skipped.extend_from_slice(raw);
                skipped.push(0);
            }
        }
        git(
            &root,
            &["read-tree", "HEAD"],
            None,
            4096,
            Duration::from_secs(60),
            token,
        )
        .await?;
        if !skipped.is_empty() {
            git(
                &root,
                &["update-index", "--skip-worktree", "-z", "--stdin"],
                Some(skipped),
                4096,
                Duration::from_secs(60),
                token,
            )
            .await?;
        }
    }
    Ok(())
}
fn finish(
    stage: Stage,
    destination: PathBuf,
    store: PathBuf,
    budget: RestoreBudget,
    token: CancellationToken,
) -> Result<Restored, Error> {
    let result = (|| {
        cancelled(&token)?;
        let by_name: HashMap<_, _> = stage
            .plan
            .items
            .iter()
            .map(|item| (item.path.as_str(), item))
            .collect();
        for path in stage.plan.links.keys() {
            let index = stage
                .plan
                .files
                .get(path)
                .and_then(|name| by_name.get(name.as_str()).copied())
                .ok_or_else(|| invalid("missing link entry"))?;
            let Kind::Link(original) = &index.kind else {
                return Err(invalid("invalid link entry"));
            };
            let output = stage.ready.path().join(path);
            fs::create_dir_all(
                output
                    .parent()
                    .ok_or_else(|| invalid("missing link parent"))?,
            )?;
            std::os::unix::fs::symlink(original, output)?;
        }
        let mut dirs = Vec::new();
        let mut pending = vec![stage.ready.path().to_path_buf()];
        let mut count = 0;
        while let Some(path) = pending.pop() {
            cancelled(&token)?;
            count += 1;
            if count > MAX_FILES * 5 {
                return Err(invalid("snapshot final entry budget exceeded"));
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                dirs.push(path.clone());
                for entry in fs::read_dir(path)? {
                    if pending.len() + count >= MAX_FILES * 5 {
                        return Err(invalid("snapshot directory entry budget exceeded"));
                    }
                    pending.push(entry?.path());
                }
            } else if metadata.is_file() {
                fs::set_permissions(
                    path,
                    fs::Permissions::from_mode(if metadata.mode() & 0o111 != 0 {
                        0o500
                    } else {
                        0o400
                    }),
                )?;
            }
        }
        if usage(stage.directory.path(), stage.block, &token)?.saturating_add(usage(
            stage.ready.path(),
            stage.block,
            &token,
        )?) > budget.work_bytes
        {
            return Err(invalid("snapshot import exceeded work budget"));
        }
        cleanup(stage.directory.path()).map_err(Error::Cleanup)?;
        for dir in dirs.iter().rev() {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o500))?;
        }
        if usage(&store, stage.block, &token)? > budget.store_bytes {
            return Err(invalid("snapshot store budget changed during restore"));
        }
        cancelled(&token)?;
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            stage.ready.path(),
            rustix::fs::CWD,
            &destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)?;
        Ok(Restored {
            repositories: stage
                .plan
                .manifest
                .repositories
                .iter()
                .map(|r| destination.join(&r.path))
                .collect(),
            directory: destination,
        })
    })();
    match result {
        Ok(restored) => {
            // Publication is the last fallible action; the scratch tree was removed first.
            let _ = stage.ready.keep();
            Ok(restored)
        }
        Err(error) => {
            cleanup(stage.directory.path()).map_err(Error::Cleanup)?;
            cleanup(stage.ready.path()).map_err(Error::Cleanup)?;
            Err(error)
        }
    }
}

async fn restore(
    archive: Vec<u8>,
    destination: &Path,
    store_root: &Path,
    budget: RestoreBudget,
    cancel: CancellationToken,
) -> Result<Restored, Error> {
    check_git().await?;
    cancelled(&cancel)?;
    let store = store_root.canonicalize()?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("missing destination parent"))?
        .canonicalize()?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("invalid snapshot destination"))?;
    let destination = parent.join(name);
    if !parent.starts_with(&store) {
        return Err(invalid(
            "snapshot destination must be fresh inside its store",
        ));
    }
    match fs::symlink_metadata(&destination) {
        Ok(_) => return Err(invalid("snapshot destination already exists")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let token = cancel.clone();
    let target = destination.clone();
    let base = store.clone();
    let stage = tokio::task::spawn_blocking(move || prepare(archive, target, base, budget, token))
        .await
        .map_err(|_| invalid("snapshot prepare worker failed"))??;
    if let Err(error) = import(&stage, &cancel).await {
        let path = stage.directory.path().to_path_buf();
        let ready = stage.ready.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            cleanup(&path)?;
            cleanup(&ready)
        })
        .await
        .map_err(|_| invalid("snapshot cleanup worker failed"))?
        .map_err(Error::Cleanup)?;
        return Err(error);
    }
    tokio::task::spawn_blocking(move || finish(stage, destination, store, budget, cancel))
        .await
        .map_err(|_| invalid("snapshot publish worker failed"))?
}

pub(super) async fn extract(
    archive: Vec<u8>,
    destination: &Path,
    store_root: &Path,
    budget: RestoreBudget,
    cancel: CancellationToken,
) -> Result<Restored, Error> {
    let token = cancel.child_token();
    let work = restore(archive, destination, store_root, budget, token.clone());
    tokio::pin!(work);
    tokio::select! {result=&mut work=>result,_=tokio::time::sleep(Duration::from_secs(180))=>{
        token.cancel();match work.await{Ok(restored)=>Ok(restored),Err(Error::Cancelled)=>Err(invalid("snapshot restore timed out")),Err(error)=>Err(error)}
    }}
}

#[cfg(test)]
mod tests;
