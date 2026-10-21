//! Codex on a host: Buzz's own copy, whether it is signed in, and keeping
//! that copy on the latest release (buzz-app's `setup.ts`). The copy lives
//! apart from any `codex` the user installed, and shares their `~/.codex`
//! sign-in and settings.

use super::rpc::{AppServer, list_models};
use crate::agent_plugins::managed::{
    self, CHECK_TIMEOUT, Fetch, Release, Setup, Tool, fetch_text, run_short,
};
use crate::agent_plugins::process::Spawner;
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use managed::{SetupState, Status as CodexStatus, Step};

pub type CodexSetup = Setup;

/// Where OpenAI publishes each release with its checksums.
const RELEASES: &str = "https://releases.openai.com/codex";
/// `uname -sm` to the release's target name. Linux builds are static
/// (musl), so they run on any Linux, NixOS included.
const TARGETS: [(&str, &str); 4] = [
    ("Darwin arm64", "aarch64-apple-darwin"),
    ("Darwin x86_64", "x86_64-apple-darwin"),
    ("Linux aarch64", "aarch64-unknown-linux-musl"),
    ("Linux x86_64", "x86_64-unknown-linux-musl"),
];

/// Codex's setup on a host whose processes `spawn` starts. A remote host
/// has no browser for the usual sign-in, so it signs in with a device
/// code, which the setup output shows.
pub fn setup(spawn: Option<Spawner>, remote: bool) -> CodexSetup {
    Setup::new(Arc::new(Codex { remote }), spawn, managed::http())
}

pub async fn check_codex(spawn: &Spawner) -> CodexStatus {
    let version = match run_short(spawn, "codex", &["--version"], BTreeMap::new()).await {
        // The launcher's answer when Buzz has no copy yet.
        Ok((Some(127), _, _)) => return CodexStatus::Missing,
        // `codex-cli 0.162.1`
        Ok((Some(0), stdout, _)) => stdout
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string(),
        Ok((_, _, stderr)) => {
            let stderr = stderr.trim();
            return CodexStatus::Error {
                message: if stderr.is_empty() {
                    "Codex did not start".into()
                } else {
                    stderr.into()
                },
            };
        }
        Err(message) => return CodexStatus::Error { message },
    };
    match run_short(spawn, "codex", &["login", "status"], BTreeMap::new()).await {
        Ok((Some(0), _, _)) => CodexStatus::Ready {
            version,
            account: None,
        },
        _ => CodexStatus::SignedOut { version },
    }
}

struct Codex {
    remote: bool,
}

impl Tool for Codex {
    fn name(&self) -> &'static str {
        "Codex"
    }
    fn origin(&self) -> &'static str {
        "releases.openai.com"
    }
    fn pin(&self) -> &'static str {
        "BUZZ_CODEX_VERSION"
    }
    fn env(&self) -> BTreeMap<String, Option<String>> {
        BTreeMap::new()
    }
    fn check(&self, spawn: Spawner) -> BoxFuture<'static, CodexStatus> {
        Box::pin(async move { check_codex(&spawn).await })
    }
    fn login(&self) -> (&'static str, Vec<String>) {
        let mut args = vec!["login".to_string()];
        if self.remote {
            args.push("--device-auth".into());
        }
        ("codex", args)
    }
    fn target(&self, uname: &str) -> Option<&'static str> {
        TARGETS
            .iter()
            .find(|(name, _)| *name == uname)
            .map(|(_, target)| *target)
    }
    fn latest(&self, fetch: Fetch) -> BoxFuture<'static, Result<Release, String>> {
        Box::pin(async move {
            let body = fetch_text(
                &fetch,
                "releases.openai.com",
                format!("{RELEASES}/channels/latest"),
            )
            .await?;
            let data: Value = serde_json::from_str(&body).unwrap_or_default();
            let version = data
                .get("tag_name")
                .and_then(Value::as_str)
                .map(|tag| tag.strip_prefix("rust-v").unwrap_or(tag).to_string())
                .unwrap_or_default();
            Ok(Release { version, data })
        })
    }
    fn checksum(
        &self,
        _: Fetch,
        release: &Release,
        target: &str,
    ) -> BoxFuture<'static, Result<Option<String>, String>> {
        let asset = format!("codex-package-{target}.tar.gz");
        let digest = release
            .data
            .get("assets")
            .and_then(Value::as_array)
            .and_then(|assets| {
                assets
                    .iter()
                    .find(|item| item.get("name").and_then(Value::as_str) == Some(&asset))
            })
            .and_then(|item| item.get("digest"))
            .and_then(Value::as_str)
            .map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest).to_string());
        Box::pin(async move { Ok(digest) })
    }
    fn test(&self, spawn: Spawner, version: String) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let (code, stdout, stderr) =
                run_short(&spawn, "codex", &["--version"], BTreeMap::new()).await?;
            if code != Some(0) || stdout.trim() != format!("codex-cli {version}") {
                let stderr = stderr.trim();
                return Err(if stderr.is_empty() {
                    "it did not report its version".into()
                } else {
                    stderr.into()
                });
            }
            let rpc = AppServer::new(None);
            let models = tokio::time::timeout(CHECK_TIMEOUT, async {
                rpc.open(&spawn, None).await?;
                list_models(&rpc).await
            })
            .await;
            rpc.close().await;
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
