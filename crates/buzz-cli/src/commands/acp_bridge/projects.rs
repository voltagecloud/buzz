//! Working directories for ACP sessions on the Paseo daemon.
//!
//! The daemon clones each project to `$HOME/code/<owner>/<repo>` and keeps
//! read-only branch views at `$HOME/code/<owner>/<repo>@<branch>`. A session
//! created in one of those directories records the project on its thread root
//! as [`PROJECT_TAG`], so `session/list` can return a working directory that
//! exists after the daemon (and its local session list) is replaced.
//! Conversations without a usable project share `$HOME/buzz`.

use std::path::{Component, Path, PathBuf};

use nostr::{Event, PublicKey};

/// Thread root tag naming the GitHub project of the session, `<owner>/<repo>`.
pub(crate) const PROJECT_TAG: &str = "paseo-project";

/// The `<owner>/<repo>` project of a client working directory: the project
/// directory itself, a `@<branch>` view of it, or a subdirectory of either.
/// Any other directory, or names GitHub does not allow, give `None`.
pub(crate) fn project_of_cwd(home: &Path, cwd: &str) -> Option<String> {
    let cwd = normalise(Path::new(cwd))?;
    let mut parts = cwd.strip_prefix(home.join("code")).ok()?.components();
    let owner = name(parts.next()?)?;
    let dir = name(parts.next()?)?;
    let repo = match dir.split_once('@') {
        Some((repo, branch)) if !branch.is_empty() => repo,
        Some(_) => return None,
        None => dir,
    };
    let project = format!("{owner}/{repo}");
    valid_project(&project).then_some(project)
}

/// The project recorded on a thread root, when it carries exactly one valid
/// [`PROJECT_TAG`] and one of the `trusted` keys (the owner and the bridge's
/// signer) authored it. Other channel members cannot choose the project.
pub(crate) fn project_of_root(root: &Event, trusted: &[PublicKey]) -> Option<String> {
    if !trusted.contains(&root.pubkey) {
        return None;
    }
    let mut values = root.tags.iter().filter_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some(PROJECT_TAG)).then(|| parts.get(1))
    });
    let value = values.next()??;
    if values.next().is_some() || !valid_project(value) {
        return None;
    }
    Some(value.clone())
}

/// The working directory `session/list` reports for a thread root; see
/// [`project_of_root`] for `trusted`. `fallback` is computed at most once
/// per listing.
pub(crate) fn session_cwd(
    home: &Path,
    root: &Event,
    trusted: &[PublicKey],
    fallback: &mut Option<PathBuf>,
) -> PathBuf {
    if let Some(project) = project_of_root(root, trusted) {
        let dir = home.join("code").join(project);
        if dir.is_dir() {
            return dir;
        }
    }
    fallback.get_or_insert_with(|| buzz_dir(home)).clone()
}

/// The `cwd` filter of `session/list`, in the form [`session_cwd`] returns:
/// a project, its branch views, and their subdirectories become the project
/// directory; `$HOME/buzz` and its subdirectories become `$HOME/buzz`.
pub(crate) fn filter_cwd(home: &Path, cwd: &str) -> Option<PathBuf> {
    if let Some(project) = project_of_cwd(home, cwd) {
        return Some(home.join("code").join(project));
    }
    let cwd = normalise(Path::new(cwd))?;
    let buzz = home.join("buzz");
    Some(if cwd.starts_with(&buzz) { buzz } else { cwd })
}

/// `$HOME/buzz`, created owner-only when missing; `$HOME` when it cannot be.
fn buzz_dir(home: &Path) -> PathBuf {
    let dir = home.join("buzz");
    if dir.is_dir() {
        return dir;
    }
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        let mut builder = builder;
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    match builder.create(&dir) {
        Ok(()) => dir,
        // Another bridge process may have created it first.
        Err(_) if dir.is_dir() => dir,
        Err(error) => {
            eprintln!(
                "acp-bridge: cannot create {}: {error}; listing sessions under {}",
                dir.display(),
                home.display()
            );
            home.to_path_buf()
        }
    }
}

/// An absolute path with `.` and `..` resolved lexically.
fn normalise(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn name(part: Component<'_>) -> Option<&str> {
    match part {
        Component::Normal(name) => name.to_str(),
        _ => None,
    }
}

/// `<owner>/<repo>` within GitHub's name rules: an owner of 1 to 39 ASCII
/// letters, digits, hyphens, or underscores (managed user accounts), not
/// starting with a hyphen; a repository of 1 to 100 ASCII letters, digits,
/// `.`, `-`, or `_`, other than `.` and `..`.
pub(crate) fn valid_project(project: &str) -> bool {
    let Some((owner, repo)) = project.split_once('/') else {
        return false;
    };
    let owner_ok = (1..=39).contains(&owner.len())
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    let repo_ok = (1..=100).contains(&repo.len())
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    owner_ok && repo_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_directories_map_to_projects() {
        let home = Path::new("/home/paseo");
        for (cwd, want) in [
            ("/home/paseo/code/block/buzz", Some("block/buzz")),
            ("/home/paseo/code/block/buzz/", Some("block/buzz")),
            ("/home/paseo/code/block/buzz/crates/x", Some("block/buzz")),
            ("/home/paseo/code/block/buzz@main", Some("block/buzz")),
            ("/home/paseo/code/block/buzz@feat/x/src", Some("block/buzz")),
            (
                "/home/paseo/code/block/my.repo_1-x",
                Some("block/my.repo_1-x"),
            ),
            ("/home/paseo/code/block/./buzz", Some("block/buzz")),
            ("/home/paseo/code/other/../block/buzz", Some("block/buzz")),
            ("/home/paseo/code/block/buzz@", None),
            ("/home/paseo/code/block", None),
            ("/home/paseo/code", None),
            ("/home/paseo/buzz", None),
            ("/home/paseo/code/-bad/buzz", None),
            ("/home/paseo/code/alice_acme/buzz", Some("alice_acme/buzz")),
            ("/home/paseo/code/bad.owner/buzz", None),
            ("/home/paseo/code/block/..", None),
            ("/home/paseo/code/block/bu zz", None),
            ("/home/other/code/block/buzz", None),
            ("code/block/buzz", None),
            ("/", None),
        ] {
            assert_eq!(project_of_cwd(home, cwd).as_deref(), want, "{cwd}");
        }
    }

    #[test]
    fn project_names_follow_github_rules() {
        for (project, want) in [
            ("block/buzz", true),
            ("a/b", true),
            (&format!("{}/r", "o".repeat(39)), true),
            (&format!("{}/r", "o".repeat(40)), false),
            (&format!("o/{}", "r".repeat(100)), true),
            (&format!("o/{}", "r".repeat(101)), false),
            ("block", false),
            ("block/buzz/extra", false),
            ("/buzz", false),
            ("block/", false),
            ("block/..", false),
            ("block/.", false),
            ("../buzz", false),
            ("bl@ck/buzz", false),
            ("block/bu@zz", false),
        ] {
            assert_eq!(valid_project(project), want, "{project}");
        }
    }
}
