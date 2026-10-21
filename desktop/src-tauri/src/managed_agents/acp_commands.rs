//! PATH-discovered `buzz-*-acp` drop-in wrappers for the stock harness, and
//! the portable-alias rule shared artifacts carry.

use super::discovery::{command_search::command_discovery_dirs, resolve_command};
use std::path::{Path, PathBuf};

fn strip_windows_command_extension(name: &str) -> &str {
    [".exe", ".bat", ".cmd"]
        .into_iter()
        .find_map(|extension| {
            name.get(name.len().saturating_sub(extension.len())..)
                .filter(|suffix| suffix.eq_ignore_ascii_case(extension))
                .map(|_| &name[..name.len() - extension.len()])
        })
        .unwrap_or(name)
}

/// Whether a transport is a portable stock or conventional wrapper alias.
/// Shared artifacts carry aliases only, never machine paths or command lines.
/// Owner-controlled native commands and owner-device sync retain legacy values.
pub(crate) fn is_portable_acp_command(command: &str) -> bool {
    if command == super::DEFAULT_ACP_COMMAND {
        return true;
    }
    command.len() <= 255
        && command
            .strip_prefix("buzz-")
            .and_then(|name| name.strip_suffix("-acp"))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            })
}

/// Reject nonportable commands at foreign catalog and snapshot boundaries.
pub(crate) fn validate_portable_acp_command(command: Option<&str>) -> Result<(), String> {
    if command.is_some_and(|command| !is_portable_acp_command(command)) {
        return Err("ACP command must be buzz-acp or a portable buzz-*-acp alias".to_string());
    }
    Ok(())
}

fn acp_command_from_filename(name: &str, require_windows_extension: bool) -> Option<&str> {
    let command = strip_windows_command_extension(name);
    if require_windows_extension && command == name {
        return None;
    }
    (command != super::DEFAULT_ACP_COMMAND && is_portable_acp_command(command)).then_some(command)
}

/// Enumerate executable `buzz-*-acp` drop-in wrappers without running them.
/// The stock `buzz-acp` command is deliberately excluded because it has no
/// namespaced middle segment and is always the editor's built-in default.
///
/// Candidate names come from the normal executable search directories, but
/// each result is resolved through the same command resolver used at spawn.
/// This guarantees the path shown for a command is the path that command will
/// execute, even when managed shims or workspace builds take precedence.
pub fn discover_acp_command_candidates() -> Vec<(String, PathBuf)> {
    discover_acp_command_candidates_in(command_discovery_dirs(), resolve_command)
}

fn discover_acp_command_candidates_in(
    dirs: impl IntoIterator<Item = PathBuf>,
    mut resolve: impl FnMut(&str) -> Option<PathBuf>,
) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let filename = entry.file_name().to_string_lossy().to_string();
            let Some(command) = acp_command_from_filename(&filename, cfg!(windows)) else {
                continue;
            };
            if !entry.path().is_file() || !is_executable(&entry.path()) {
                continue;
            }
            if seen.insert(command.to_string()) {
                if let Some(path) = resolve(command) {
                    results.push((command.to_string(), path));
                }
            }
        }
    }

    results.sort_by(|left, right| left.0.cmp(&right.0));
    results
}

/// Check if a file is executable (Unix: mode bits; other platforms: always true).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// A PATH-discovered drop-in wrapper for the stock `buzz-acp` harness.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpCommandCandidate {
    pub command: String,
    pub binary_path: String,
}

#[cfg(test)]
#[path = "acp_commands_tests.rs"]
mod tests;
