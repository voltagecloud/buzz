//! The developer's agent-only commit signing key. Enrollment generates an
//! Ed25519 key with `ssh-keygen`, registers its public half on the
//! developer's GitHub account as a signing key, and sends the private half in
//! the profile, so Sandbox commits are signed and verified as theirs without
//! their own GPG key leaving this device. Nothing here logs a token or a key.

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

/// Registers `public` on the token's account as a signing key, and returns
/// its id.
pub async fn register(token: &str, title: &str, public: &str) -> Result<u64, String> {
    let body = serde_json::json!({ "title": title, "key": public }).to_string();
    let response = request(reqwest::Method::POST, "/user/ssh_signing_keys", token)?
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| "Could not reach GitHub.".to_string())?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Err(
            "GitHub would not add the signing key. Give the token Account permissions → SSH signing keys: Read and write."
                .into(),
        );
    }
    if !status.is_success() {
        return Err(format!(
            "GitHub would not add the signing key ({}).",
            status.as_u16()
        ));
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| "Could not read GitHub's reply.".to_string())?;
    let key: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Could not read GitHub's reply.".to_string())?;
    key["id"]
        .as_u64()
        .ok_or_else(|| "Could not read GitHub's reply.".to_string())
}

/// Removes signing key `id`, best effort: an enrollment that did not finish
/// leaves no key behind on the account.
pub async fn unregister(token: &str, id: u64) {
    if let Ok(request) = request(
        reqwest::Method::DELETE,
        &format!("/user/ssh_signing_keys/{id}"),
        token,
    ) {
        let _ = request.send().await;
    }
}
