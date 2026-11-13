//! Agent file paths in replayed NIP-AT transcripts.
//!
//! A remote agent's tool calls name files in its sandbox (for example
//! `/home/agent/backend/src/main.rs`), which do not exist on the Paseo
//! daemon. A transcript's `workspace` lists the agent's GitHub checkouts at
//! turn end. When a path lies in a checkout whose branch was pushed, replay
//! points it at the daemon's read-only view of that branch,
//! `$HOME/code/<owner>/<repo>@<branch>` (see [`super::views`]). Other paths
//! stay unchanged.
//!
//! Live telemetry carries no workspace, and during a turn the agent's edits
//! are usually not pushed yet, so live paths are never rewritten.

use std::path::{Component, Path, PathBuf};

use buzz_sdk::transcript::WorkspaceEntry;
use serde_json::{json, Value};

use super::projects;

/// `rawInput` fields that hold a file path in common agent tools.
const RAW_INPUT_PATH_FIELDS: &[&str] = &[
    "path",
    "file_path",
    "filePath",
    "abs_path",
    "absPath",
    "notebook_path",
];

/// Longest branch name the daemon's `paseo-code view` accepts.
const MAX_BRANCH_BYTES: usize = 201;

/// A pushed branch the replay points paths at. `repo` is the lowercase
/// `<owner>/<repo>`, as `paseo-code` names its checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ViewKey {
    pub repo: String,
    pub branch: String,
    pub head: String,
}

impl ViewKey {
    /// The daemon directory of this branch view.
    pub(crate) fn dir(&self, home: &Path) -> Option<PathBuf> {
        let (owner, repo) = self.repo.split_once('/')?;
        Some(
            home.join("code")
                .join(owner)
                .join(format!("{repo}@{}", self.branch)),
        )
    }
}

/// Prepare one replayed `session/update` for the client: rewrite file paths
/// in pushed checkouts, turn diffs that the publisher shortened into display
/// text, and drop fields the publisher replaced with an elision marker where
/// ACP requires another type. Returns the branch views the rewritten paths
/// point at.
pub(crate) fn prepare_update(
    update: &mut Value,
    workspace: &[WorkspaceEntry],
    home: &Path,
) -> Vec<ViewKey> {
    let mut views = Vec::new();
    if !matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    ) {
        return views;
    }
    let Some(map) = update.as_object_mut() else {
        return views;
    };
    if map.get("_meta").is_some_and(|meta| !meta.is_object()) {
        map.remove("_meta");
    }
    let mut rewrite = |value: &mut Value| {
        let Some(raw) = value.as_str() else {
            return;
        };
        if let Some((path, view)) = rewrite_path(raw, workspace, home) {
            *value = Value::String(path);
            if !views.contains(&view) {
                views.push(view);
            }
        }
    };
    if let Some(locations) = map.get_mut("locations").and_then(Value::as_array_mut) {
        for location in locations {
            if let Some(path) = location.get_mut("path") {
                rewrite(path);
            }
        }
    }
    if let Some(input) = map.get_mut("rawInput").and_then(Value::as_object_mut) {
        for field in RAW_INPUT_PATH_FIELDS {
            if let Some(path) = input.get_mut(*field) {
                rewrite(path);
            }
        }
    }
    match map.get_mut("content") {
        Some(Value::Array(items)) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("diff") {
                    continue;
                }
                if let Some(path) = item.get_mut("path") {
                    rewrite(path);
                }
                if shortened_diff(item) {
                    *item = display_only_diff(item);
                }
            }
        }
        Some(_) => {
            map.remove("content");
        }
        None => {}
    }
    views
}

/// The daemon path of an agent `path` and the view it lies in, when the
/// innermost checkout containing it has a pushed, git-safe branch. Paths
/// with `..`, relative paths, and paths outside every checkout give `None`.
pub(crate) fn rewrite_path(
    raw: &str,
    workspace: &[WorkspaceEntry],
    home: &Path,
) -> Option<(String, ViewKey)> {
    let path = Path::new(raw);
    if !path.is_absolute() || has_parent_dir(path) {
        return None;
    }
    // The innermost checkout owns the file, even when it is not pushed and
    // an enclosing one is.
    let (entry, rest) = workspace
        .iter()
        .filter(|entry| !has_parent_dir(Path::new(&entry.path)))
        .filter_map(|entry| {
            path.strip_prefix(&entry.path)
                .ok()
                .map(|rest| (entry, rest))
        })
        .max_by_key(|(entry, _)| Path::new(&entry.path).components().count())?;
    if !entry.pushed {
        return None;
    }
    let branch = entry.branch.as_deref().filter(|b| valid_view_branch(b))?;
    let repo = entry.repo.to_ascii_lowercase();
    if !projects::valid_project(&repo) {
        return None;
    }
    let view = ViewKey {
        repo,
        branch: branch.to_owned(),
        head: entry.head.clone(),
    };
    let mut out = view.dir(home)?;
    for part in rest.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if !out.starts_with(home.join("code")) {
        return None;
    }
    Some((out.to_str()?.to_owned(), view))
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part, Component::ParentDir))
}

/// A branch name that is a valid git ref name and that the daemon's
/// `paseo-code view` accepts: ASCII letters, digits, `_`, `.`, `/`, and `-`,
/// starting with a letter or digit; no `..`, `//`, trailing `/` or `.`, path
/// component starting with `.`, or component ending in `.lock`.
pub(crate) fn valid_view_branch(branch: &str) -> bool {
    branch.len() <= MAX_BRANCH_BYTES
        && branch
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-'))
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.ends_with('/')
        && !branch.ends_with('.')
        && !branch
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"))
}

/// Whether a diff's text was shortened or redacted by the publisher, so it is
/// no longer the real change (NIP-AT: a reader must not apply it).
fn shortened_diff(diff: &Value) -> bool {
    ["oldText", "newText"].iter().any(|field| {
        diff.get(*field)
            .and_then(Value::as_str)
            .is_some_and(|text| {
                text.contains("[redacted]")
                    || text
                        .find("…[elided ")
                        .is_some_and(|start| text[start..].contains(" bytes]…"))
            })
    })
}

/// A shortened diff as plain text content, which ACP clients show but do not
/// offer to apply.
fn display_only_diff(diff: &Value) -> Value {
    let text = |field: &str| diff.get(field).and_then(Value::as_str).unwrap_or_default();
    let path = diff
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("unknown file");
    json!({
        "type": "content",
        "content": {
            "type": "text",
            "text": format!(
                "Shortened diff of {path} (display only, cannot be applied)\n--- before\n{}\n+++ after\n{}",
                text("oldText"),
                text("newText"),
            ),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pushed_checkout_paths_are_rewritten() {
        let entry = WorkspaceEntry {
            path: "/sandbox/repo".into(),
            repo: "voltagecloud/backend".into(),
            branch: Some("feat/links".into()),
            head: "a".repeat(40),
            pushed: true,
        };
        for (raw, pushed, branch, nested, expected) in [
            (
                "/sandbox/repo/src/lib.rs",
                true,
                "feat/links",
                false,
                Some("src/lib.rs"),
            ),
            ("/sandbox/repo/src/lib.rs", false, "feat/links", false, None),
            ("/sandbox/repo/src/lib.rs", true, "../escape", false, None),
            ("/sandbox/repo/../secret", true, "feat/links", false, None),
            ("sandbox/repo/src/lib.rs", true, "feat/links", false, None),
            (
                "/sandbox/repository/src/lib.rs",
                true,
                "feat/links",
                false,
                None,
            ),
            ("/sandbox/repo/vendor/a", true, "feat/links", true, None),
        ] {
            let mut entry = entry.clone();
            entry.pushed = pushed;
            entry.branch = Some(branch.into());
            let mut workspace = vec![entry];
            if nested {
                workspace.push(WorkspaceEntry {
                    path: "/sandbox/repo/vendor".into(),
                    pushed: false,
                    ..workspace[0].clone()
                });
            }
            let actual = rewrite_path(raw, &workspace, Path::new("/home/paseo"));
            assert_eq!(
                actual.map(|(path, _)| path),
                expected.map(|suffix| format!(
                    "/home/paseo/code/voltagecloud/backend@feat/links/{suffix}"
                )),
                "{raw}"
            );
        }
    }

    #[test]
    fn view_branches_are_git_safe() {
        for (branch, want) in [
            ("main", true),
            ("feat/x", true),
            ("release-1.2_rc", true),
            (&"b".repeat(201), true),
            (&"b".repeat(202), false),
            ("", false),
            ("-x", false),
            ("/x", false),
            (".x", false),
            ("feat/.x", false),
            ("a..b", false),
            ("a//b", false),
            ("a/", false),
            ("a.", false),
            ("a.lock", false),
            ("a.lock/b", false),
            ("a b", false),
            ("a~1", false),
            ("a^", false),
            ("a:b", false),
            ("a@{1}", false),
            ("a\\b", false),
            ("a\nb", false),
        ] {
            assert_eq!(valid_view_branch(branch), want, "{branch:?}");
        }
    }
}
