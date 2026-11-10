//! The local tool boundary: the GitHub CLI (`gh`), `ssh-keygen`, `git`,
//! `claude`, and GitHub's `GET /user`. Each call mirrors the GPUI enroll
//! form (`remote_workspace/signing.rs` and `enroll_form.rs`): the same
//! commands, arguments, timeouts and failure wording. No token or private
//! key is ever logged or put on a command line.

use std::process::Stdio;
use std::time::Duration;

use zeroize::Zeroizing;

use super::profile::GitHubIdentity;

/// A generated key pair: the OpenSSH private key and its public line.
pub struct SigningKey {
    /// The OpenSSH private key. Never print it.
    pub private: Zeroizing<String>,
    /// The `ssh-ed25519 …` public line.
    pub public: String,
}

/// What `buzz onboard` runs on the developer's machine.
pub trait Tools {
    /// `gh auth token`, when `gh` is installed and signed in.
    async fn gh_auth_token(&self) -> Option<Zeroizing<String>>;

    /// Who `token` belongs to, from GitHub's `GET /user`.
    async fn github_identity(&self, token: &str) -> Result<GitHubIdentity, String>;

    /// A fresh Ed25519 key without a passphrase, from `ssh-keygen`.
    async fn ssh_keygen(&self, comment: &str) -> Result<SigningKey, String>;

    /// Adds `public` to `login`'s account as a signing key titled `title`
    /// with `gh`, which must be signed in as `login`; or why not.
    async fn gh_add_signing_key(
        &self,
        login: &str,
        title: &str,
        public: &str,
    ) -> Result<(), &'static str>;

    /// `git config --global user.email`, or empty.
    async fn git_email(&self) -> String;

    /// Runs `claude setup-token` attached to the terminal, so the developer
    /// signs in and sees the token; false when `claude` is not installed.
    async fn claude_setup_token(&self) -> bool;
}

/// The real tools.
pub struct SystemTools;

const API: &str = "https://api.github.com";
/// How long one `gh` call may take.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

async fn gh(args: &[&str]) -> Result<std::process::Output, &'static str> {
    let child = tokio::process::Command::new("gh")
        .args(args)
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(GH_TIMEOUT, child).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(_)) => Err("the GitHub CLI (gh) is not installed"),
        Err(_) => Err("the GitHub CLI (gh) did not answer"),
    }
}

/// Why a failed `gh` call could not add the key, from its error output.
pub fn gh_failure(stderr: &str) -> &'static str {
    if stderr.contains("admin:ssh_signing_key") {
        "gh needs the admin:ssh_signing_key scope: run gh auth refresh -h github.com -s admin:ssh_signing_key, then buzz onboard again"
    } else if stderr.contains("gh auth login") || stderr.contains("not logged") {
        "gh is not signed in: run gh auth login"
    } else if stderr.contains("key is already in use") {
        "this key is already on a GitHub account"
    } else {
        "gh could not add the key"
    }
}

/// `GET /user`'s login and name.
pub fn parse_identity(body: &[u8]) -> Result<GitHubIdentity, String> {
    let user: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "Could not read GitHub's reply.".to_string())?;
    let login = user["login"]
        .as_str()
        .filter(|login| !login.is_empty())
        .ok_or("Could not read GitHub's reply.")?
        .to_owned();
    let name = user["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
        .unwrap_or(&login)
        .to_owned();
    Ok(GitHubIdentity { login, name })
}

impl Tools for SystemTools {
    async fn gh_auth_token(&self) -> Option<Zeroizing<String>> {
        let output = gh(&["auth", "token", "--hostname", "github.com"])
            .await
            .ok()?;
        let token = Zeroizing::new(String::from_utf8(output.stdout).ok()?.trim().to_owned());
        (output.status.success() && !token.is_empty()).then_some(token)
    }

    async fn github_identity(&self, token: &str) -> Result<GitHubIdentity, String> {
        let unreachable = || "Could not reach GitHub.".to_string();
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| unreachable())?;
        let response = client
            .get(format!("{API}/user"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "buzz-cli")
            .send()
            .await
            .map_err(|_| unreachable())?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("GitHub did not accept the token.".into());
        }
        if !response.status().is_success() {
            return Err(format!(
                "GitHub refused the token ({}).",
                response.status().as_u16()
            ));
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| "Could not read GitHub's reply.".to_string())?;
        parse_identity(&body)
    }

    async fn ssh_keygen(&self, comment: &str) -> Result<SigningKey, String> {
        let directory = tempfile::tempdir().map_err(|_| "Could not create the signing key.")?;
        let path = directory.path().join("agent-signing-key");
        let status = tokio::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| "Could not run ssh-keygen. Install OpenSSH.")?;
        if !status.success() {
            return Err("ssh-keygen could not create the signing key.".into());
        }
        let private = Zeroizing::new(
            std::fs::read_to_string(&path).map_err(|_| "Could not read the signing key.")?,
        );
        let public = std::fs::read_to_string(path.with_extension("pub"))
            .map_err(|_| "Could not read the signing key.")?
            .trim()
            .to_owned();
        Ok(SigningKey { private, public })
    }

    async fn gh_add_signing_key(
        &self,
        login: &str,
        title: &str,
        public: &str,
    ) -> Result<(), &'static str> {
        let user = gh(&["api", "user", "--jq", ".login"]).await?;
        if !user.status.success() {
            return Err(gh_failure(&String::from_utf8_lossy(&user.stderr)));
        }
        if !String::from_utf8_lossy(&user.stdout)
            .trim()
            .eq_ignore_ascii_case(login)
        {
            return Err("gh is signed in to a different GitHub account than the token");
        }
        let title = format!("title={title}");
        let key = format!("key={public}");
        let added = gh(&[
            "api",
            "--method",
            "POST",
            "/user/ssh_signing_keys",
            "-f",
            &title,
            "-f",
            &key,
        ])
        .await?;
        if added.status.success() {
            Ok(())
        } else {
            Err(gh_failure(&String::from_utf8_lossy(&added.stderr)))
        }
    }

    async fn git_email(&self) -> String {
        tokio::process::Command::new("git")
            .args(["config", "--global", "user.email"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|email| email.trim().to_owned())
            .unwrap_or_default()
    }

    async fn claude_setup_token(&self) -> bool {
        tokio::process::Command::new("claude")
            .arg("setup-token")
            .status()
            .await
            .is_ok_and(|status| status.success())
    }
}
