//! The developer's agent-only commit signing key. Enrollment generates an
//! Ed25519 key with `ssh-keygen` and sends the private half in the profile, so
//! Sandbox commits are signed as the developer without their own GPG key
//! leaving this device. Once the manager stores the profile, the public half
//! is added to the developer's GitHub account as a signing key with their
//! local GitHub CLI (`gh`), or they add it themselves; either way the agents'
//! GitHub token needs no account permission. Nothing here logs a token or a
//! private key.

use std::time::Duration;
use zeroize::Zeroizing;

const API: &str = "https://api.github.com";

/// Who a GitHub token belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct GitHubIdentity {
    pub login: String,
    /// The profile name, or the login when the account has none.
    pub name: String,
}

/// A generated key pair: the OpenSSH private key and its public line.
pub struct SigningKey {
    pub private: Zeroizing<String>,
    pub public: String,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "Could not reach GitHub.".to_string())
}

fn request(
    method: reqwest::Method,
    path: &str,
    token: &str,
) -> Result<reqwest::RequestBuilder, String> {
    Ok(client()?
        .request(method, format!("{API}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "buzz-gpui"))
}

/// The identity of `token`'s account, from `GET /user`.
pub async fn identity(token: &str) -> Result<GitHubIdentity, String> {
    let response = request(reqwest::Method::GET, "/user", token)?
        .send()
        .await
        .map_err(|_| "Could not reach GitHub.".to_string())?;
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

/// A fresh Ed25519 key, without a passphrase, from `ssh-keygen`.
pub async fn generate(comment: &str) -> Result<SigningKey, String> {
    let comment = comment.to_owned();
    tokio::task::spawn_blocking(move || {
        let directory = tempfile::tempdir().map_err(|_| "Could not create the signing key.")?;
        let path = directory.path().join("agent-signing-key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| "Could not run ssh-keygen. Install OpenSSH.")?;
        if !status.success() {
            return Err("ssh-keygen could not create the signing key.");
        }
        let private = Zeroizing::new(
            std::fs::read_to_string(&path).map_err(|_| "Could not read the signing key.")?,
        );
        let public = std::fs::read_to_string(path.with_extension("pub"))
            .map_err(|_| "Could not read the signing key.")?
            .trim()
            .to_owned();
        Ok(SigningKey { private, public })
    })
    .await
    .map_err(|_| "Could not create the signing key.".to_string())?
    .map_err(str::to_string)
}

/// How the public half reached GitHub.
#[derive(Clone, Debug, PartialEq)]
pub enum Registration {
    /// Added with the local GitHub CLI.
    Added,
    /// For the developer to add, and why `gh` could not.
    Manual {
        public: String,
        reason: &'static str,
    },
}

/// How long one `gh` call may take.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

async fn gh(args: &[&str]) -> Result<std::process::Output, &'static str> {
    let child = tokio::process::Command::new("gh")
        .args(args)
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
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
        "gh needs the admin:ssh_signing_key scope: run gh auth refresh -h github.com -s admin:ssh_signing_key, then enroll again"
    } else if stderr.contains("gh auth login") || stderr.contains("not logged") {
        "gh is not signed in: run gh auth login"
    } else if stderr.contains("key is already in use") {
        "this key is already on a GitHub account"
    } else {
        "gh could not add the key"
    }
}

/// Adds `public` to `login`'s GitHub account as a signing key titled
/// `title`, with the local GitHub CLI signed in as that account.
pub async fn add_with_gh(login: &str, title: &str, public: &str) -> Result<(), &'static str> {
    let user = gh(&["api", "user", "--jq", ".login"]).await?;
    if !user.status.success() {
        return Err(gh_failure(&String::from_utf8_lossy(&user.stderr)));
    }
    let signed_in = String::from_utf8_lossy(&user.stdout);
    if !signed_in.trim().eq_ignore_ascii_case(login) {
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

/// Adds the key with `gh`, or hands it back for the developer to add.
pub async fn register(login: &str, title: &str, public: &str) -> Registration {
    match add_with_gh(login, title, public).await {
        Ok(()) => Registration::Added,
        Err(reason) => Registration::Manual {
            public: public.to_owned(),
            reason,
        },
    }
}
