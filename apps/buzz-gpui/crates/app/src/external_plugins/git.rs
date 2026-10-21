//! Adding plugins from a Git repository (buzz-app's `prepare_git` in
//! `crates/plugin-manager/src/imports.rs`).
//!
//! The repository is cloned once, shallow, into a scratch folder, and its
//! plugin folders are listed for the user to choose from. Nothing in it
//! runs: Git reads no global or system config, runs no hooks, follows no
//! submodules, asks for no passwords and speaks only HTTPS and SSH. Links
//! are checked out as plain files. A clone may take 60 seconds and 256
//! MiB. The chosen folder is copied into this app's plugin folder, so the
//! plugin runs from a copy the user picked, not from the repository.

use super::protocol::{MANIFEST, Manifest};
use super::release;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(60);
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_CANDIDATES: usize = 32;
const MAX_DEPTH: usize = 8;
const SKIP: [&str; 4] = ["node_modules", "target", "__pycache__", "venv"];

/// A plugin folder found in the repository.
pub struct Candidate {
    /// Relative to the repository root, `/`-separated; empty for the root.
    pub folder: String,
    pub manifest: Result<Manifest, String>,
    /// Its publisher, if signed.
    pub signed: Result<Option<String>, String>,
}

/// A cloned repository and the plugins in it.
pub struct Fetched {
    scratch: tempfile::TempDir,
    pub repository: String,
    pub reference: String,
    pub commit: String,
    pub candidates: Vec<Candidate>,
}

impl Fetched {
    fn root(&self) -> PathBuf {
        self.scratch.path().join("repository")
    }

    pub fn candidate(&self, folder: &str) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.folder == folder)
    }

    /// Copies the plugin at `folder` to `into`, replacing what is there.
    pub fn copy(&self, folder: &str, into: &Path) -> Result<(), String> {
        if self.candidate(folder).is_none() {
            return Err("That folder is not a plugin in this repository.".into());
        }
        let source = if folder.is_empty() {
            self.root()
        } else {
            self.root().join(folder)
        };
        let parent = into.parent().ok_or("No folder to install into")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let name = into
            .file_name()
            .ok_or("No folder to install into")?
            .to_string_lossy()
            .into_owned();
        let staged = parent.join(format!(".{name}.new-{}", uuid::Uuid::new_v4()));
        copy_tree(&source, &staged).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&staged);
        })?;
        let old = parent.join(format!(".{name}.old-{}", uuid::Uuid::new_v4()));
        if into.exists() {
            std::fs::rename(into, &old).map_err(|error| error.to_string())?;
        }
        if let Err(error) = std::fs::rename(&staged, into) {
            // Put the old copy back.
            let _ = std::fs::rename(&old, into);
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error.to_string());
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() == ".git" {
            continue;
        }
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// A branch or tag as buzz-app accepts it; empty is the default branch.
fn reference(input: &str) -> Result<String, String> {
    let reference = input.trim();
    if !reference.is_empty()
        && (reference.starts_with('-')
            || reference.len() > 200
            || !reference
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c)))
    {
        return Err(
            "Enter a branch or tag using letters, digits, dots, underscores, slashes or \
                    hyphens."
                .into(),
        );
    }
    Ok(reference.to_owned())
}

struct Git {
    scratch: tempfile::TempDir,
    deadline: Instant,
    /// A community repository's URL and its NIP-98 token. Git sends the
    /// header only to that URL, and follows no redirects.
    authorization: Option<(String, String)>,
}

impl Git {
    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.env_clear();
        // SSH agent sign-in and finding programs still work; Git's own
        // config, credential helpers and prompts do not.
        for key in ["PATH", "HOME", "USERPROFILE", "SYSTEMROOT", "SSH_AUTH_SOCK"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        command
            .env("PATH", buzz_desktop::process::effective_path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .env(
                "GIT_SSH_COMMAND",
                format!("ssh -F {null} -oBatchMode=yes -oStrictHostKeyChecking=yes"),
            )
            .args([
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.ssh.allow=always",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.symlinks=false",
                "-c",
                "http.followRedirects=false",
                "-c",
                &format!("core.hooksPath={null}"),
            ])
            .current_dir(self.scratch.path())
            .stdin(Stdio::null());
        if let Some((url, token)) = &self.authorization {
            // In the environment, not argv, so `ps` does not show it.
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("http.{url}.extraHeader"))
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Nostr {token}"),
                );
        }
        command
    }

    /// Runs Git, killing it at the deadline or the size limit.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = tempfile::tempfile().map_err(|error| error.to_string())?;
        let errors = tempfile::tempfile().map_err(|error| error.to_string())?;
        let mut command = self.command();
        command
            .args(args)
            .stdout(output.try_clone().map_err(|error| error.to_string())?)
            .stderr(errors.try_clone().map_err(|error| error.to_string())?);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(|error| {
            format!("Could not start Git: {error}. Install Git to add plugins from a repository.")
        })?;
        let result = loop {
            if Instant::now() >= self.deadline {
                break Err("The repository took over 60 seconds.".to_owned());
            }
            if size(self.scratch.path()) > MAX_BYTES {
                break Err("The repository is over 256 MiB.".to_owned());
            }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break read(output),
                Ok(Some(_)) => {
                    let text = read(errors).unwrap_or_default();
                    break Err(format!(
                        "Git could not read this repository: {}",
                        text.trim().chars().take(400).collect::<String>()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => break Err(error.to_string()),
            }
        };
        if result.is_err() {
            #[cfg(unix)]
            // SAFETY: signals the process group this function started.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

fn read(mut file: std::fs::File) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut text = String::new();
    file.take(1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    Ok(text)
}

fn size(root: &Path) -> u64 {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0;
    while let Some(path) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                bytes += metadata.len();
            }
        }
    }
    bytes
}

/// Finds plugin folders under `root`.
fn discover(root: &Path) -> Result<Vec<Candidate>, String> {
    let mut candidates = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0)];
    let mut seen = 0;
    while let Some((directory, prefix, depth)) = stack.pop() {
        if directory.join(MANIFEST).is_file() {
            let manifest = std::fs::read_to_string(directory.join(MANIFEST))
                .map_err(|error| error.to_string())
                .and_then(|text| Manifest::parse(&text));
            let signed = match &manifest {
                Ok(manifest) => release::verify(&directory, &manifest.id),
                Err(_) => Ok(None),
            };
            candidates.push(Candidate {
                folder: prefix.trim_end_matches('/').to_owned(),
                manifest,
                signed,
            });
            if candidates.len() > MAX_CANDIDATES {
                return Err(format!("The repository has over {MAX_CANDIDATES} plugins."));
            }
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err("The repository has too many files to search.".into());
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push((entry.path(), format!("{prefix}{name}/"), depth + 1));
            }
        }
    }
    candidates.sort_by(|a, b| a.folder.cmp(&b.folder));
    Ok(candidates)
}

/// Clones `repository` at `reference` and lists its plugins. Blocking.
/// `authorization` is a NIP-98 token signed for exactly this repository,
/// for a private repository a community hosts.
pub fn fetch(
    repository: &str,
    reference_input: &str,
    authorization: Option<String>,
) -> Result<Fetched, String> {
    let url = buzzodz_plugins::imports::repository_url(repository)?;
    let reference = reference(reference_input)?;
    if let Some(token) = &authorization
        && (token.is_empty()
            || token.len() > 8192
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte)))
    {
        return Err("The repository authorization is not valid.".into());
    }
    let git = Git {
        scratch: tempfile::tempdir().map_err(|error| error.to_string())?,
        deadline: Instant::now() + DEADLINE,
        authorization: authorization.map(|token| (url.clone(), token)),
    };
    let mut args = vec![
        "clone",
        "--depth=1",
        "--single-branch",
        "--no-tags",
        "--template=",
    ];
    if !reference.is_empty() {
        args.extend(["--branch", &reference]);
    }
    args.extend(["--", &url, "repository"]);
    git.run(&args)?;
    let commit = git
        .run(&["-C", "repository", "rev-parse", "HEAD"])?
        .trim()
        .to_owned();
    let candidates = discover(&git.scratch.path().join("repository"))?;
    if candidates.is_empty() {
        return Err(format!(
            "No plugins here: a plugin folder has a {MANIFEST}."
        ));
    }
    Ok(Fetched {
        scratch: git.scratch,
        repository: url,
        reference,
        commit,
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn plugin(root: &Path, folder: &str, id: &str) {
        let path = root.join(folder);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join(MANIFEST),
            format!(r#"{{ "id": "{id}", "name": "{id}", "command": ["./run"] }}"#),
        )
        .unwrap();
        std::fs::write(path.join("run"), "#!/bin/sh\n").unwrap();
    }

    /// A checkout with two plugins and one in a skipped folder.
    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        plugin(root.path(), "plugins/one", "example.one");
        plugin(root.path(), "two", "example.two");
        plugin(root.path(), "node_modules/skip", "example.skip");
        std::fs::write(root.path().join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref").unwrap();
        root
    }

    #[test]
    fn references_are_checked() {
        assert_eq!(reference(" v1.2 "), Ok("v1.2".into()));
        assert_eq!(reference(""), Ok(String::new()));
        assert!(reference("-x").is_err());
        assert!(reference("a b").is_err());
    }

    #[test]
    fn plugins_are_found_and_copied() {
        let root = repository();
        let candidates = discover(root.path()).unwrap();
        let folders: Vec<_> = candidates.iter().map(|c| c.folder.as_str()).collect();
        assert_eq!(folders, ["plugins/one", "two"]);
        assert!(
            candidates
                .iter()
                .all(|c| c.manifest.is_ok() && c.signed == Ok(None))
        );

        // Copying goes through a scratch clone; build one by hand here.
        let scratch = tempfile::tempdir().unwrap();
        copy_tree(root.path(), &scratch.path().join("repository")).unwrap();
        let fetched = Fetched {
            scratch,
            repository: "https://example.com/r.git".into(),
            reference: String::new(),
            commit: "abc".into(),
            candidates,
        };
        let home = tempfile::tempdir().unwrap();
        let into = home.path().join("installed/example.one");
        fetched.copy("plugins/one", &into).unwrap();
        assert!(into.join(MANIFEST).is_file() && into.join("run").is_file());
        assert!(!into.join(".git").exists());
        // Copying again replaces the old copy.
        std::fs::write(into.join("stale"), "x").unwrap();
        fetched.copy("plugins/one", &into).unwrap();
        assert!(!into.join("stale").exists());
        assert!(fetched.copy("node_modules/skip", &into).is_err());
        let left: Vec<_> = std::fs::read_dir(home.path().join("installed"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(left, ["example.one"]);
    }

    #[test]
    fn a_community_token_goes_only_to_its_repository() {
        let url = "https://relay.example.com/git/abc/plugins.git".to_owned();
        let git = Git {
            scratch: tempfile::tempdir().unwrap(),
            deadline: Instant::now() + DEADLINE,
            authorization: Some((url.clone(), "dG9rZW4=".into())),
        };
        let command = git.command();
        let env: std::collections::HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect();
        assert_eq!(env["GIT_CONFIG_KEY_0"], format!("http.{url}.extraHeader"));
        assert_eq!(env["GIT_CONFIG_VALUE_0"], "Authorization: Nostr dG9rZW4=");
        // Never on the command line.
        assert!(
            !command
                .get_args()
                .any(|arg| arg.to_string_lossy().contains("dG9rZW4="))
        );
        let refused = fetch(&url, "", Some("not a token!".into())).err();
        assert!(refused.is_some_and(|error| error.contains("not valid")));
    }

    #[test]
    fn local_and_unknown_transports_are_refused() {
        for input in [
            "/tmp/repo",
            "file:///tmp/repo",
            "http://example.com/r.git",
            "ext::sh",
        ] {
            assert!(fetch(input, "", None).is_err(), "{input}");
        }
    }
}
