//! Raw tar iteration bounds extension payloads before the tar crate can expand them.
use crate::sandbox_archive::ArchiveError as Error;
use buzz_sdk::sandbox_attach::snapshot::{
    archive::{inspect_bundle, Inventory},
    MAX_BLOB_BYTES, MAX_FILES, MAX_SOURCE_BYTES,
};
use flate2::bufread::GzDecoder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_ENTRIES: usize = MAX_FILES * 3;
const MAX_PAX: usize = 16384;
const MAX_META: usize = 1024 * 1024;
fn invalid(message: &'static str) -> Error {
    Error::Invalid(message)
}

fn check(cancel: &CancellationToken, start: Instant) -> Result<(), Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if start.elapsed() > Duration::from_secs(120) {
        return Err(invalid("snapshot preflight timed out"));
    }
    Ok(())
}

pub(super) fn safe(value: &str) -> Result<PathBuf, Error> {
    if value.is_empty()
        || value.len() > 4096
        || value.split('/').count() > 128
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        || value
            .split('/')
            .any(|s| s.is_empty() || matches!(s, "." | ".."))
    {
        return Err(invalid("unsafe snapshot path"));
    }
    Ok(PathBuf::from(value))
}
pub(super) fn excluded(path: &Path) -> bool {
    const NAMES: &[&str] = &[
        ".git",
        ".aws",
        ".ssh",
        ".gnupg",
        ".config",
        ".codex",
        ".claude",
        ".cache",
        ".local",
        ".npm",
        ".cargo",
        ".rustup",
        ".venv",
        "venv",
        "node_modules",
        "target",
        "__pycache__",
        "dist",
        "build",
        ".terraform",
        "credentials",
        "credentials.json",
        "auth.json",
        "id_rsa",
        "id_ed25519",
        ".npmrc",
        ".netrc",
        ".pypirc",
        ".git-credentials",
    ];
    path.iter().any(|s| {
        s.to_str().is_none_or(|s| {
            s.eq_ignore_ascii_case(".git")
                || NAMES.contains(&s)
                || s.starts_with(".env")
                || [
                    ".pem",
                    ".key",
                    ".p12",
                    ".pfx",
                    ".tfstate",
                    ".tfstate.backup",
                ]
                .iter()
                .any(|suffix| s.ends_with(suffix))
        })
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub version: u8,
    pub snapshot: bool,
    pub scanned: bool,
    pub repositories: Vec<Repo>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Repo {
    pub path: String,
    pub head: String,
    pub base: String,
    pub branch: String,
    pub shallow: Vec<String>,
    pub(super) files: Vec<String>,
    pub excluded: Vec<String>,
    pub deleted: Vec<String>,
}
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Dir,
    Link(String),
}
#[derive(Clone)]
pub(super) struct Item {
    pub path: String,
    pub size: u64,
    pub kind: Kind,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub mode: u32,
}
/// Validated snapshot structure; parsing performs no filesystem writes or Git execution.
/// Consumers must still validate imported Git objects and enforce local capacity limits.
pub struct ArchivePlan {
    pub(super) manifest: Manifest,
    pub(super) items: Vec<Item>,
    pub(super) inventory: HashMap<usize, Inventory>,
    pub(super) files: HashMap<PathBuf, String>,
    pub(super) links: HashMap<PathBuf, PathBuf>,
    pub(super) bytes: u64,
}

struct Bounded<R> {
    inner: R,
    read: u64,
    start: Instant,
    cancel: CancellationToken,
}
impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() || self.start.elapsed() > Duration::from_secs(120) {
            return Err(std::io::Error::other(
                "snapshot read cancelled or timed out",
            ));
        }
        let n = self.inner.read(buf)?;
        self.read = self.read.saturating_add(n as u64);
        if self.read > MAX_SOURCE_BYTES + MAX_ENTRIES as u64 * 1024 {
            return Err(std::io::Error::other("snapshot tar expansion exceeded"));
        }
        Ok(n)
    }
}
fn pax(bytes: &[u8]) -> Result<HashMap<String, String>, Error> {
    let mut values = HashMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tail = &bytes[offset..];
        let space = tail
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| invalid("invalid PAX record"))?;
        let count = std::str::from_utf8(&tail[..space])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > space + 2 && *n <= tail.len())
            .ok_or_else(|| invalid("invalid PAX length"))?;
        let record = &tail[space + 1..count];
        if record.last() != Some(&b'\n') {
            return Err(invalid("invalid PAX terminator"));
        }
        let value = std::str::from_utf8(&record[..record.len() - 1])
            .map_err(|_| invalid("non-UTF8 PAX record"))?;
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| invalid("invalid PAX field"))?;
        if !matches!(key, "path" | "linkpath")
            || value.len() > 4096
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            return Err(invalid("unsupported or duplicate PAX field"));
        }
        offset += count;
    }
    Ok(values)
}

pub(super) fn walk<F>(bytes: &[u8], cancel: &CancellationToken, mut visit: F) -> Result<(), Error>
where
    F: FnMut(Item, &mut dyn Read) -> Result<(), Error>,
{
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(invalid("snapshot archive exceeds byte budget"));
    }
    let input = Bounded {
        inner: GzDecoder::new(bytes),
        read: 0,
        start: Instant::now(),
        cancel: cancel.clone(),
    };
    let mut archive = tar::Archive::new(input);
    let mut pending = None;
    let mut metadata = 0usize;
    let mut entries = 0;
    let mut last_end = 0u64;
    // raw(true) prevents tar from calling read_all on GNU/PAX extension bodies.
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(invalid("too many snapshot archive entries"));
        }
        let ty = entry.header().entry_type();
        let size = entry.size();
        last_end = entry
            .raw_file_position()
            .checked_add(size.div_ceil(512) * 512)
            .ok_or_else(|| invalid("snapshot tar offset overflow"))?;
        if ty.is_pax_local_extensions() {
            if pending.is_some() || size > MAX_PAX as u64 {
                return Err(invalid("snapshot PAX payload exceeds budget"));
            }
            metadata = metadata
                .checked_add(512 + size.div_ceil(512) as usize * 512)
                .ok_or_else(|| invalid("snapshot metadata overflow"))?;
            if metadata > MAX_META {
                return Err(invalid("snapshot PAX metadata exceeds budget"));
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            pending = Some(pax(&data)?);
            continue;
        }
        if !(ty.is_file() || ty.is_dir() || ty.is_symlink()) || size > MAX_SOURCE_BYTES {
            return Err(invalid("unsupported snapshot archive entry"));
        }
        let mut fields = pending.take().unwrap_or_default();
        let path = match fields.remove("path") {
            Some(path) => path,
            None => std::str::from_utf8(&entry.path_bytes())
                .map_err(|_| invalid("non-UTF8 snapshot path"))?
                .to_owned(),
        };
        let path = if ty.is_dir() {
            path.strip_suffix('/').unwrap_or(&path).to_owned()
        } else {
            path
        };
        safe(&path)?;
        let kind = if ty.is_symlink() {
            if size != 0 {
                return Err(invalid("snapshot link has data"));
            }
            let link = match fields.remove("linkpath") {
                Some(link) => link,
                None => entry
                    .link_name_bytes()
                    .and_then(|v| std::str::from_utf8(&v).ok().map(str::to_owned))
                    .ok_or_else(|| invalid("missing snapshot link"))?,
            };
            Kind::Link(link)
        } else if ty.is_dir() {
            if size != 0 {
                return Err(invalid("snapshot directory has data"));
            }
            Kind::Dir
        } else {
            Kind::File
        };
        if !fields.is_empty() {
            return Err(invalid("PAX link metadata on non-link"));
        }
        let item = Item {
            path,
            size,
            kind,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            mode: entry.header().mode()?,
        };
        visit(item, &mut entry)?;
        std::io::copy(&mut entry, &mut std::io::sink())?;
    }
    if pending.is_some() {
        return Err(invalid("dangling snapshot PAX metadata"));
    }
    let mut reader = archive.into_inner();
    let mut tail = Vec::new();
    reader.by_ref().take(10241).read_to_end(&mut tail)?;
    if tail.len() > 10240 || tail.iter().any(|b| *b != 0) {
        return Err(invalid("snapshot tar trailing data"));
    }
    // Finish gzip CRC/trailer checks and reject concatenated gzip members.
    let mut extra = [0u8; 1];
    if reader.read(&mut extra)? != 0
        || reader.read.saturating_sub(last_end) < 1024
        || !reader.inner.get_ref().is_empty()
    {
        return Err(invalid("snapshot gzip trailing data"));
    }
    Ok(())
}
fn location(path: &str) -> Option<(usize, &str)> {
    let tail = path.strip_prefix("snapshot/repositories/")?;
    let (index, rest) = tail.split_once('/')?;
    let parsed = index.parse::<usize>().ok()?;
    (index == parsed.to_string()).then_some((parsed, rest))
}
fn resolve_link<'a>(
    source: &Path,
    root: &Path,
    target: &'a str,
    links: &'a HashMap<PathBuf, (PathBuf, String)>,
) -> Result<PathBuf, Error> {
    let mut current = source
        .parent()
        .ok_or_else(|| invalid("missing link parent"))?
        .to_path_buf();
    let mut pending: VecDeque<_> = target.split('/').collect();
    let mut expansions = 0;
    while let Some(part) = pending.pop_front() {
        match part {
            "" | "." => {}
            ".." => {
                if current == root || !current.pop() {
                    return Err(invalid("snapshot link escapes repository"));
                }
            }
            name => {
                current.push(name);
                if let Some((_, next)) = links.get(&current) {
                    expansions += 1;
                    if expansions > 128 {
                        return Err(invalid("snapshot link chain exceeded"));
                    }
                    current.pop();
                    for part in next.split('/').rev() {
                        pending.push_front(part);
                    }
                }
            }
        }
        if pending.len() > 256
            || current.components().count() > 256
            || current.as_os_str().len() > 4096
        {
            return Err(invalid("snapshot link expansion exceeded"));
        }
    }
    Ok(current)
}

/// Validate an authenticated snapshot before allocating storage or importing its packs.
/// Tar/PAX metadata, paths, object expansion, repository/file counts and symlink chains
/// are bounded; unscanned archives, external links and unsupported types are refused.
pub fn inspect_archive(bytes: &[u8], cancel: &CancellationToken) -> Result<ArchivePlan, Error> {
    let started = Instant::now();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut manifest = None;
    let mut inventory = HashMap::new();
    let mut total = 0u64;
    walk(bytes, cancel, |item, reader| {
        if !seen.insert(item.path.clone()) {
            return Err(invalid("duplicate snapshot archive path"));
        }
        total = total
            .checked_add(item.size)
            .filter(|n| *n <= MAX_SOURCE_BYTES)
            .ok_or_else(|| invalid("snapshot source budget exceeded"))?;
        if item.path == "snapshot/manifest.json" {
            if item.kind != Kind::File || item.size > 8 * 1024 * 1024 {
                return Err(invalid("invalid snapshot manifest"));
            }
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            manifest = Some(
                serde_json::from_slice::<Manifest>(&data)
                    .map_err(|_| invalid("invalid snapshot manifest"))?,
            );
        } else if let Some((index, "history.bundle")) = location(&item.path) {
            if index >= 64 || item.kind != Kind::File || item.size > MAX_BLOB_BYTES as u64 {
                return Err(invalid("invalid snapshot history entry"));
            }
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            inventory.insert(index, inspect_bundle(&data, MAX_SOURCE_BYTES)?);
        }
        items.push(item);
        Ok(())
    })
    .map_err(|e| {
        if cancel.is_cancelled() {
            Error::Cancelled
        } else {
            e
        }
    })?;
    let manifest = manifest.ok_or_else(|| invalid("missing snapshot manifest"))?;
    if manifest.version != 1
        || !manifest.snapshot
        || !manifest.scanned
        || manifest.repositories.is_empty()
        || manifest.repositories.len() > 64
        || inventory.len() != manifest.repositories.len()
    {
        return Err(invalid("unsupported or unscanned snapshot manifest"));
    }
    let mut files = HashMap::new();
    let mut links = HashMap::new();
    let mut roots = HashSet::new();
    let mut declared = HashSet::new();
    let mut count = 0usize;
    let mut expanded = 0u64;
    let mut objects = 0u32;
    let mut refs = 0usize;
    for (index, repo) in manifest.repositories.iter().enumerate() {
        check(cancel, started)?;
        let root = safe(&repo.path)?;
        let stats = inventory
            .get(&index)
            .ok_or_else(|| invalid("missing snapshot bundle"))?;
        if !roots.insert(root.clone())
            || excluded(&root)
            || repo.base != "bundle"
            || repo.head.len() != stats.hash_bytes * 2
            || !repo.head.bytes().all(|b| b.is_ascii_hexdigit())
            || repo.branch.len() > 1024
            || repo.shallow.len() > MAX_FILES
            || !repo
                .shallow
                .iter()
                .all(|s| s.len() == repo.head.len() && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("invalid snapshot repository metadata"));
        }
        expanded = expanded
            .checked_add(stats.expanded_bytes)
            .filter(|n| *n <= MAX_SOURCE_BYTES)
            .ok_or_else(|| invalid("snapshot aggregate expansion exceeded"))?;
        objects = objects
            .checked_add(stats.objects)
            .filter(|n| *n <= 200000)
            .ok_or_else(|| invalid("snapshot aggregate object count exceeded"))?;
        refs += stats.refs.len();
        let mut ref_names = HashSet::new();
        for (_, name) in &stats.refs {
            safe(name)?;
            if !ref_names.insert(name) || name.to_ascii_lowercase().starts_with("refs/replace/") {
                return Err(invalid("duplicate or unsafe snapshot reference"));
            }
        }
        count += repo.files.len() + repo.excluded.len() + repo.deleted.len();
        if count > MAX_FILES || refs > MAX_FILES {
            return Err(invalid("snapshot path count exceeded"));
        }
        for value in repo.excluded.iter().chain(repo.deleted.iter()) {
            safe(value)?;
        }
        for file in &repo.files {
            let relative = safe(file)?;
            if excluded(&relative)
                || !declared.insert(format!("snapshot/repositories/{index}/files/{file}"))
            {
                return Err(invalid("invalid copied snapshot path"));
            }
        }
    }
    for item in &items {
        check(cancel, started)?;
        if item.kind == Kind::Dir {
            continue;
        }
        if item.path == "snapshot/manifest.json" {
            continue;
        }
        let (index, tail) =
            location(&item.path).ok_or_else(|| invalid("unknown snapshot archive entry"))?;
        let repo = manifest
            .repositories
            .get(index)
            .ok_or_else(|| invalid("unknown snapshot repository"))?;
        if tail == "history.bundle" {
            continue;
        }
        let relative = tail
            .strip_prefix("files/")
            .ok_or_else(|| invalid("unknown snapshot archive entry"))?;
        if !declared.remove(&item.path) {
            return Err(invalid("undeclared copied snapshot file"));
        }
        let full = Path::new(&repo.path).join(safe(relative)?);
        // Nested repositories may both name the same physical working file.
        // The deepest repository owns its final copied view.
        let replace = files.get(&full).is_none_or(|old: &String| {
            location(old)
                .is_some_and(|(old, _)| manifest.repositories[old].path.len() < repo.path.len())
        });
        if replace {
            files.insert(full, item.path.clone());
        }
    }
    if !declared.is_empty() {
        return Err(invalid("snapshot manifest file is missing"));
    }
    let mut directories = HashSet::new();
    for item in &items {
        if item.kind != Kind::Dir {
            for parent in Path::new(&item.path).ancestors().skip(1) {
                directories.insert(parent.to_path_buf());
            }
        }
    }
    for index in 0..manifest.repositories.len() {
        directories.insert(PathBuf::from(format!(
            "snapshot/repositories/{index}/files"
        )));
    }
    if items
        .iter()
        .any(|item| item.kind == Kind::Dir && !directories.contains(Path::new(&item.path)))
    {
        return Err(invalid("undeclared snapshot directory"));
    }
    let by_name: HashMap<_, _> = items
        .iter()
        .map(|item| (item.path.as_str(), item))
        .collect();
    let mut raw_links = HashMap::new();
    for (full, name) in &files {
        check(cancel, started)?;
        let item = by_name
            .get(name.as_str())
            .ok_or_else(|| invalid("missing selected snapshot item"))?;
        if let Kind::Link(target) = &item.kind {
            if target.is_empty()
                || target.len() > 4096
                || target.starts_with('/')
                || target.split('/').count() > 128
                || target
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\\' | ':'))
            {
                return Err(invalid("unsafe snapshot link target"));
            }
            let (index, _) = location(name).ok_or_else(|| invalid("invalid link scope"))?;
            raw_links.insert(
                full.clone(),
                (
                    PathBuf::from(&manifest.repositories[index].path),
                    target.clone(),
                ),
            );
        }
    }
    for file in files.keys() {
        check(cancel, started)?;
        if file
            .ancestors()
            .skip(1)
            .any(|parent| files.contains_key(parent))
        {
            return Err(invalid("snapshot file is an ancestor"));
        }
    }
    for root in &roots {
        if root.ancestors().any(|parent| files.contains_key(parent)) {
            return Err(invalid("snapshot repository crosses a file"));
        }
    }
    for (source, (root, target)) in &raw_links {
        check(cancel, started)?;
        links.insert(
            source.clone(),
            resolve_link(source, root, target, &raw_links)?,
        );
    }
    Ok(ArchivePlan {
        manifest,
        items,
        inventory,
        files,
        links,
        bytes: total,
    })
}

impl ArchivePlan {
    /// Repository paths relative to the eventual mirror directory.
    pub fn repositories(&self) -> impl Iterator<Item = &str> {
        self.manifest.repositories.iter().map(|r| r.path.as_str())
    }
    /// Total packed object count across all repositories.
    pub fn objects(&self) -> u32 {
        self.inventory.values().map(|s| s.objects).sum()
    }
    /// Total reconstructed Git object bytes, including delta targets.
    pub fn expanded_bytes(&self) -> u64 {
        self.inventory.values().map(|s| s.expanded_bytes).sum()
    }
    /// Number of unique copied working files, including symlinks.
    pub fn files(&self) -> usize {
        self.files.len()
    }
    /// Number of selected safe symlinks.
    pub fn symlinks(&self) -> usize {
        self.links.len()
    }
    /// Number of bounded tar entries, excluding PAX metadata headers.
    pub fn entries(&self) -> usize {
        self.items.len()
    }
    /// Declared regular payload bytes, including copied bundles and manifest.
    pub fn uncompressed_bytes(&self) -> u64 {
        self.bytes
    }
}
#[cfg(test)]
pub(in crate::sandbox_archive) mod tests;
