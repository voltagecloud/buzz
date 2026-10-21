//! Claude on a host: Buzz's own copy, whether it is signed in, and keeping
//! that copy on the latest release (buzz-app's `setup.ts`). The copy lives
//! apart from any `claude` the user installed, and shares their
//! `~/.claude` sign-in and settings.

use super::claude::{ClaudeProcess, Launch, env};
use crate::agent_plugins::managed::{
    self, CHECK_TIMEOUT, Fetch, Release, Setup, Tool, fetch_text, run_short,
};
use crate::agent_plugins::process::Spawner;
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use managed::{SetupState, Status as ClaudeStatus, Step};

pub type ClaudeSetup = Setup;

/// Where Anthropic publishes each release with its checksums.
const RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";
/// The oldest release Buzz drives: MCP servers marked `alwaysLoad`, which
/// the Buzz tools are, arrived in 2.1.121.
pub const MIN_VERSION: &str = "2.1.121";
/// `uname -sm` to the release's platform name.
const PLATFORMS: [(&str, &str); 4] = [
    ("Darwin arm64", "darwin-arm64"),
    ("Darwin x86_64", "darwin-x64"),
    ("Linux aarch64", "linux-arm64"),
    ("Linux x86_64", "linux-x64"),
];

/// Claude's setup on a host whose processes `spawn` starts.
pub fn setup(spawn: Option<Spawner>) -> ClaudeSetup {
    Setup::new(Arc::new(Claude), spawn, managed::http())
}

/// [`setup`] as [`managed::Copies`] makes it.
pub fn for_host(spawn: Option<Spawner>, _remote: bool) -> ClaudeSetup {
    setup(spawn)
}

pub async fn check_claude(spawn: &Spawner) -> ClaudeStatus {
    let version = match run_short(spawn, "claude", &["--version"], env()).await {
        // The launcher's answer when Buzz has no copy yet.
        Ok((Some(127), _, _)) => return ClaudeStatus::Missing,
        Ok((Some(0), stdout, _)) => stdout
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        Ok((_, _, stderr)) => {
            let stderr = stderr.trim();
            return ClaudeStatus::Error {
                message: if stderr.is_empty() {
                    "Claude did not start".into()
                } else {
                    stderr.into()
                },
            };
        }
        Err(message) => return ClaudeStatus::Error { message },
    };
    if managed::compare_versions(&version, MIN_VERSION).is_lt() {
        return ClaudeStatus::Outdated { version };
    }
    let Ok((_, stdout, _)) = run_short(spawn, "claude", &["auth", "status", "--json"], env()).await
    else {
        return ClaudeStatus::SignedOut { version };
    };
    let Ok(status) = serde_json::from_str::<serde_json::Value>(&stdout) else {
        return ClaudeStatus::SignedOut { version };
    };
    if status.get("loggedIn") != Some(&serde_json::Value::Bool(true)) {
        return ClaudeStatus::SignedOut { version };
    }
    let account = ["email", "orgName"]
        .iter()
        .filter_map(|key| status.get(*key).and_then(serde_json::Value::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    ClaudeStatus::Ready {
        version,
        account: (!account.is_empty()).then_some(account),
    }
}

struct Claude;

impl Tool for Claude {
    fn name(&self) -> &'static str {
        "Claude"
    }
    fn origin(&self) -> &'static str {
        "downloads.claude.ai"
    }
    fn pin(&self) -> &'static str {
        "BUZZ_CLAUDE_VERSION"
    }
    fn env(&self) -> BTreeMap<String, Option<String>> {
        env()
    }
    fn min_version(&self) -> Option<&'static str> {
        Some(MIN_VERSION)
    }
    fn check(&self, spawn: Spawner) -> BoxFuture<'static, ClaudeStatus> {
        Box::pin(async move { check_claude(&spawn).await })
    }
    fn login(&self) -> (&'static str, Vec<String>) {
        ("claude", vec!["auth".into(), "login".into()])
    }
    fn target(&self, uname: &str) -> Option<&'static str> {
        PLATFORMS
            .iter()
            .find(|(name, _)| *name == uname)
            .map(|(_, platform)| *platform)
    }
    fn latest(&self, fetch: Fetch) -> BoxFuture<'static, Result<Release, String>> {
        Box::pin(async move {
            let latest =
                fetch_text(&fetch, "downloads.claude.ai", format!("{RELEASES}/latest")).await?;
            Ok(Release {
                version: latest.trim().to_string(),
                data: serde_json::Value::Null,
            })
        })
    }
    fn checksum(
        &self,
        fetch: Fetch,
        release: &Release,
        target: &str,
    ) -> BoxFuture<'static, Result<Option<String>, String>> {
        let url = format!("{RELEASES}/{}/manifest.json", release.version);
        let target = target.to_string();
        Box::pin(async move {
            let manifest = fetch_text(&fetch, "downloads.claude.ai", url).await?;
            let manifest: serde_json::Value = serde_json::from_str(&manifest)
                .map_err(|_| "downloads.claude.ai sent an invalid manifest".to_string())?;
            Ok(manifest
                .pointer(&format!("/platforms/{target}/checksum"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string))
        })
    }
    fn test(&self, spawn: Spawner, version: String) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let (code, stdout, stderr) = run_short(&spawn, "claude", &["--version"], env()).await?;
            if code != Some(0) || !stdout.starts_with(&version) {
                let stderr = stderr.trim();
                return Err(if stderr.is_empty() {
                    "it did not report its version".into()
                } else {
                    stderr.into()
                });
            }
            let launch = Launch {
                cwd: "~".into(),
                ..Launch::default()
            };
            let claude = ClaudeProcess::spawn(&spawn, &launch).await?;
            let models = tokio::time::timeout(CHECK_TIMEOUT, async {
                claude.initialize(&launch).await?;
                Ok::<_, String>(claude.models().await)
            })
            .await;
            claude.kill().await;
            match models {
                Ok(Ok(models)) if !models.is_empty() => Ok(()),
                Ok(Err(error)) => Err(error),
                _ => Err("it did not list its models".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests;
