//! A dedicated device login, separate from the user's normal Codex cache.
//! Only the one-time device code is shown. The private cache is removed
//! when the login completes, fails, times out, or is cancelled.

use std::process::Stdio;
use std::time::Duration;

#[cfg(windows)]
use buzz_desktop::process::windows_job::WindowsJob;
use buzz_desktop::process::{effective_path, resolve_program};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use regex::Regex;
use serde_json::{Value, from_str};
use tempfile::Builder as TempDirBuilder;
use tokio::fs::{metadata, read_to_string};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tokio::{pin, select};
use zeroize::Zeroizing;

const MAX_BYTES: usize = 16_384;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(16 * 60);

/// A completed sign-in remains bound to the Buzz identity and community
/// that requested it, including after the asynchronous task has completed.
pub struct PendingLogin {
    cache: Zeroizing<String>,
    owner: String,
    relay: String,
}

impl PendingLogin {
    /// Bind a dedicated cache to its enrollment scope.
    pub fn new(cache: Zeroizing<String>, owner: String, relay: String) -> Self {
        Self {
            cache,
            owner,
            relay,
        }
    }

    /// Consume only for the same identity and community.
    pub fn into_cache(self, owner: &str, relay: &str) -> Result<Zeroizing<String>, String> {
        if self.owner != owner || self.relay != relay {
            return Err("Buzz identity or community changed; sign in to ChatGPT again.".into());
        }
        Ok(self.cache)
    }
}

#[cfg(unix)]
struct LoginGroup(Pid);

#[cfg(unix)]
impl Drop for LoginGroup {
    fn drop(&mut self) {
        // The group is created before spawn and contains the npm wrapper
        // and its native child. Killing only the wrapper leaks that child.
        let _ = killpg(self.0, Signal::SIGKILL);
    }
}

/// Cancelling the view's future also cancels its runtime login task.
pub struct LoginTask(pub JoinHandle<Result<Zeroizing<String>, String>>);

impl Drop for LoginTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn prompt(output: &[u8]) -> String {
    let plain = String::from_utf8_lossy(output);
    let plain = Regex::new(r"\x1b\[[0-9;]*m")
        .map(|ansi| ansi.replace_all(&plain, "").into_owned())
        .unwrap_or_default();
    let code = Regex::new(r"\b[A-Z0-9]{4}-[A-Z0-9]{4,6}\b")
        .ok()
        .and_then(|code| code.find(&plain).map(|found| found.as_str().to_string()));
    match code {
        Some(code) => format!(
            "Open https://auth.openai.com/codex/device and enter {code}. Complete only the sign-in you started here."
        ),
        None => "Requesting a ChatGPT sign-in code…".into(),
    }
}

/// Sign in with the installed CLI without inheriting other model credentials.
pub async fn run(progress: Sender<String>) -> Result<Zeroizing<String>, String> {
    let path = effective_path();
    let mut command = Command::new(resolve_program("codex", &path));
    command.env("PATH", path);
    run_command(progress, command).await
}

async fn run_command(
    progress: Sender<String>,
    mut command: Command,
) -> Result<Zeroizing<String>, String> {
    let directory = TempDirBuilder::new()
        .prefix("buzz-codex-login-")
        .tempdir()
        .map_err(|_| "Could not create a private login directory.".to_string())?;
    command
        .args([
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth",
        ])
        .env("CODEX_HOME", directory.path())
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
        .env_remove("CODEX_CA_CERTIFICATE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let _job = WindowsJob::new().ok_or("Could not contain the Codex sign-in process.")?;
    #[cfg(windows)]
    let mut child = _job
        .spawn_hidden(&mut command)
        .ok_or("Could not start a contained Codex sign-in process.")?;
    #[cfg(not(windows))]
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start Codex. Install the Codex CLI and try again.".to_string())?;
    #[cfg(unix)]
    let _group = LoginGroup(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw)
            .ok_or("Could not contain the Codex sign-in process.")?,
    );
    let mut output = child
        .stdout
        .take()
        .ok_or("Could not read Codex's sign-in prompt.")?;
    let mut captured = Zeroizing::new(Vec::new());
    let mut chunk = [0_u8; 1024];
    let mut closed = false;
    let deadline = sleep(LOGIN_TIMEOUT);
    pin!(deadline);
    let mut polling = interval(Duration::from_millis(500));
    let status = loop {
        select! {
            _ = &mut deadline => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err("ChatGPT sign-in timed out. Start a new sign-in.".into());
            }
            _ = polling.tick() => {
                if progress.is_closed() {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err("ChatGPT sign-in was cancelled.".into());
                }
                let _ = progress.try_send(prompt(&captured));
                if let Some(status) = child.try_wait().map_err(|_| "Could not check ChatGPT sign-in.".to_string())? {
                    break status;
                }
            }
            read = output.read(&mut chunk), if !closed => {
                let count = read.map_err(|_| "Could not read ChatGPT sign-in progress.".to_string())?;
                if count == 0 {
                    closed = true;
                } else if captured.len().saturating_add(count) > MAX_BYTES {
                    return Err("Codex returned too much sign-in output.".into());
                } else {
                    captured.extend_from_slice(&chunk[..count]);
                }
            }
        }
    };
    if !status.success() {
        return Err("ChatGPT sign-in did not finish. Check that device-code login is enabled for your workspace, then try again.".into());
    }
    let path = directory.path().join("auth.json");
    let size = metadata(&path)
        .await
        .map_err(|_| "Codex did not save a login.".to_string())?
        .len();
    if size > MAX_BYTES as u64 {
        return Err("Codex returned an oversized login.".into());
    }
    let cache = Zeroizing::new(
        read_to_string(path)
            .await
            .map_err(|_| "Could not read the dedicated Codex login.".to_string())?,
    );
    let value: Value = from_str(&cache).map_err(|_| "Codex saved an invalid login.".to_string())?;
    if value["auth_mode"] != "chatgpt"
        || !value["OPENAI_API_KEY"].is_null()
        || value["tokens"]["refresh_token"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err("Codex did not save a ChatGPT subscription login.".into());
    }
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod process {
        use super::super::*;
        use std::fs::{read_to_string as read_file, set_permissions, write};
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tempfile::{TempDir, tempdir};
        use tokio::sync::mpsc::channel;
        use tokio::time::timeout;
        use tokio::{spawn, test as async_test};

        fn executable(tail: &str) -> (TempDir, Command) {
            let directory = tempdir().unwrap();
            let path = directory.path().join("codex-fixture");
            let script = format!(
                r#"#!/bin/sh
printf '%s\n%s\n' "$CODEX_HOME" "$$" > "$(dirname "$0")/record"
{tail}
"#
            );
            write(&path, script).unwrap();
            set_permissions(&path, PermissionsExt::from_mode(0o700)).unwrap();
            (directory, Command::new(path))
        }

        #[async_test]
        async fn a_completed_login_uses_and_removes_a_dedicated_cache() {
            let (directory, command) = executable(
                r#"
printf '%s' '{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"refresh_token":"private-fixture"}}' > "$CODEX_HOME/auth.json"
printf 'ABCD-EFGHJ\n'
"#,
            );
            let (sender, _receiver) = channel(1);
            let cache = run_command(sender, command).await.unwrap();
            assert!(cache.contains("private-fixture"));
            let record = read_file(directory.path().join("record")).unwrap();
            assert!(!Path::new(record.lines().next().unwrap()).exists());
        }

        #[async_test]
        async fn dropping_the_login_task_kills_the_cli_and_removes_its_cache() {
            let (directory, command) = executable(
                r#"sleep 60 &
printf '%s\n' "$!" >> "$(dirname "$0")/record"
wait"#,
            );
            let (sender, _receiver) = channel(1);
            let task = spawn(run_command(sender, command));
            let record = timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(record) = read_file(directory.path().join("record"))
                        && record.lines().count() == 3
                    {
                        break record;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let mut lines = record.lines();
            let cache = lines.next().unwrap();
            let pids: Vec<_> = lines.collect();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!Path::new(cache).exists());
            timeout(Duration::from_secs(5), async {
                loop {
                    let mut alive = false;
                    for pid in &pids {
                        alive |= Command::new("kill")
                            .args(["-0", *pid])
                            .stderr(Stdio::null())
                            .status()
                            .await
                            .unwrap()
                            .success();
                    }
                    if !alive {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn a_completed_login_cannot_be_enrolled_under_another_identity_or_community() {
        for (owner, relay, accepted) in [
            ("alice", "community-a", true),
            ("bob", "community-a", false),
            ("alice", "community-b", false),
        ] {
            let login = PendingLogin::new(
                Zeroizing::new("private-cache".into()),
                "alice".into(),
                "community-a".into(),
            );
            assert_eq!(login.into_cache(owner, relay).is_ok(), accepted);
        }
    }

    #[test]
    fn only_the_device_code_and_fixed_openai_url_reach_the_form() {
        let text = prompt(
            b"private-token-must-not-display\n\x1b[34mABCD-EFGHJ\x1b[0m\nhttps://attacker.invalid",
        );
        assert!(text.contains("ABCD-EFGHJ"));
        assert!(text.contains("https://auth.openai.com/codex/device"));
        assert!(!text.contains("private-token"));
        assert!(!text.contains("attacker"));
    }
}
