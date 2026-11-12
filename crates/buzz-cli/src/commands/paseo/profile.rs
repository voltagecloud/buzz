//! Collect the developer profile that `enroll` stores, from a terminal.
//!
//! Only non-secret values are prefilled: the Git email from `git config`, the
//! GitHub login and name from the token the developer types, and the shared
//! via profile. Every credential is typed without echo, sent only inside the
//! encrypted enroll request, and never written to disk or output.

use std::io::{BufRead, Write};

use serde_json::{json, Map, Value};

use crate::error::CliError;

/// The shared via profile from `voltage/remote-agents.md` ("Shared via
/// profile"). It holds only `op://` references. A test keeps it identical to
/// the runbook.
pub(super) const SHARED_VIA_PROFILE: &str = include_str!("shared-via-profile.toml");

const GITHUB_HELP: &str = "\
GitHub token for your agents (fine-grained).
  Create it at https://github.com/settings/personal-access-tokens/new
  Resource owner: voltagecloud. Repository access: All repositories.
  Contents and Pull requests: Read and write. Workflows: Read and write only if
  agents may change .github/workflows. Account permissions: none.
  If the organization requires approval, the token stays pending until an owner approves it.";

const OP_HELP: &str = "\
1Password service account token (ops_...), so agents can read the Buzz Agents Dev vault.
  1Password: Developer -> Infrastructure Secrets Management -> Other -> Service Account.
  Give it read access to Buzz Agents Dev only, and leave the expiry at Never.
  No permission to create one? Ask a 1Password admin to create it and share the token in 1Password.
  Guide: https://developer.1password.com/docs/service-accounts/get-started/";

/// Model credentials in prompt order: (profile key, label, where to get it).
const MODELS: [(&str, &str, &str); 5] = [
    (
        "claude_oauth_token",
        "Claude subscription token (sk-ant-oat01-...)",
        "Run `claude setup-token`, sign in to claude.ai with your Voltage account, and approve.\n  \
         Your Paseo assistant needs this token.",
    ),
    (
        "anthropic_api_key",
        "Anthropic API key",
        "Only if you do not use a Claude subscription token: https://console.anthropic.com/settings/keys",
    ),
    (
        "codex_access_token",
        "Codex access token (at-...)",
        "Workspace admins create it at https://chatgpt.com/admin/access-tokens (Codex scope, longest validity).",
    ),
    (
        "openai_api_key",
        "OpenAI API key",
        "Only if you do not use a Codex access token: https://platform.openai.com/api-keys",
    ),
    (
        "openrouter_api_key",
        "OpenRouter API key",
        "Optional, for the Buzz agent harness: https://openrouter.ai/keys",
    ),
];

/// Pairs of model credentials a profile may hold only one of.
const EXCLUSIVE: [(&str, &str); 2] = [
    ("claude_oauth_token", "anthropic_api_key"),
    ("codex_access_token", "openai_api_key"),
];

pub(super) const PASEO_GIT_HELP: &str = "\
Paseo GitHub read token (optional): lets your Paseo daemon read code.
  Create a separate fine-grained token at https://github.com/settings/personal-access-tokens/new
  Repository access: only the repositories Paseo should read.
  Contents and Pull requests: Read-only. Nothing else.
  Only your daemon receives it; it never enters your agent profile.";

/// Terminal input. A trait so tests can script a session.
pub(super) trait Prompt {
    /// Print guidance to the developer.
    fn note(&mut self, text: &str);
    /// Read a line; empty input returns `default`.
    fn line(&mut self, label: &str, default: Option<&str>) -> Result<String, CliError>;
    /// Read a value without echo; empty input returns `None`.
    fn secret(&mut self, label: &str) -> Result<Option<String>, CliError>;
}

/// The interactive terminal: guidance and prompts on stderr, secrets
/// without echo.
pub(super) struct Terminal;

impl Prompt for Terminal {
    fn note(&mut self, text: &str) {
        eprintln!("\n{text}");
    }

    fn line(&mut self, label: &str, default: Option<&str>) -> Result<String, CliError> {
        match default {
            Some(default) => eprint!("{label} [{default}]: "),
            None => eprint!("{label}: "),
        }
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut input)
            .map_err(|error| CliError::Other(format!("could not read input: {error}")))?;
        let input = input.trim();
        Ok(if input.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            input.to_owned()
        })
    }

    fn secret(&mut self, label: &str) -> Result<Option<String>, CliError> {
        let value = rpassword::prompt_password(format!("{label}: "))
            .map_err(|error| CliError::Other(format!("could not read input: {error}")))?;
        let value = value.trim().to_owned();
        Ok((!value.is_empty()).then_some(value))
    }
}

/// Values the developer's machine already knows.
pub(super) struct Local {
    pub git_email: Option<String>,
    /// The via profile to enroll; the shared one unless overridden.
    pub via_profile: String,
    /// Whether the developer named the via profile (`--via-profile`), so an
    /// update replaces the stored one.
    pub via_explicit: bool,
}

impl Local {
    pub(super) fn detect(via_profile: Option<String>) -> Self {
        let git_email = std::process::Command::new("git")
            .args(["config", "--global", "user.email"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|email| email.trim().to_owned())
            .filter(|email| !email.is_empty());
        Self {
            git_email,
            via_explicit: via_profile.is_some(),
            via_profile: via_profile.unwrap_or_else(|| SHARED_VIA_PROFILE.to_owned()),
        }
    }
}

/// The GitHub account a token belongs to.
pub(super) struct GithubUser {
    pub login: String,
    pub name: Option<String>,
}

/// Ask GitHub which account `token` belongs to, as the manager will.
pub(super) async fn github_user(api: &str, token: &str) -> Result<GithubUser, CliError> {
    let response = reqwest::Client::new()
        .get(format!("{}/user", api.trim_end_matches('/')))
        .bearer_auth(token)
        .header("User-Agent", "buzz-cli")
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| CliError::Other("could not reach GitHub to check the token; retry".into()))?;
    if !response.status().is_success() {
        return Err(CliError::Usage(
            "GitHub did not accept the token. Create a new fine-grained token and try again".into(),
        ));
    }
    let user: Value = response
        .json()
        .await
        .map_err(|_| CliError::Other("GitHub returned an unexpected response".into()))?;
    let login = user["login"]
        .as_str()
        .filter(|login| !login.is_empty())
        .ok_or_else(|| CliError::Other("GitHub returned no login for the token".into()))?;
    Ok(GithubUser {
        login: login.to_owned(),
        name: user["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned),
    })
}

/// Creates the agents' signing key for a GitHub login or developer ID.
pub(super) type Keygen = fn(&str) -> Result<SigningKey, CliError>;

/// A new Ed25519 commit-signing key for agents only.
pub(super) struct SigningKey {
    pub private: String,
    pub public: String,
}

/// Generate the agents' signing key with `ssh-keygen`, in a private
/// temporary directory that is removed on return.
pub(super) fn generate_signing_key(login: &str) -> Result<SigningKey, CliError> {
    let directory = tempfile::tempdir().map_err(|error| {
        CliError::Other(format!("could not create a temporary directory: {error}"))
    })?;
    let path = directory.path().join("buzz-agent-signing");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C"])
        .arg(format!("buzz-agents {login}"))
        .arg("-f")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|_| {
            CliError::Usage("ssh-keygen is required to create your agents' signing key".into())
        })?;
    if !status.success() {
        return Err(CliError::Other(
            "ssh-keygen could not create a signing key".into(),
        ));
    }
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path).map_err(|error| {
            CliError::Other(format!("could not read the new signing key: {error}"))
        })
    };
    Ok(SigningKey {
        private: read(&path)?,
        public: read(&path.with_extension("pub"))?.trim().to_owned(),
    })
}

/// Add the agents' public signing key to the developer's GitHub account with
/// `gh`, returning whether it worked. On failure, show the manual steps.
pub(super) fn add_signing_key(prompt: &mut dyn Prompt, public: &str) -> bool {
    let added = run_gh_with_deadline(public, std::time::Duration::from_secs(60));
    if added {
        prompt
            .note("Added your agents' signing key to GitHub; their commits will show as Verified.");
    } else {
        prompt.note(&format!(
            "Add your agents' signing key to GitHub so their commits show as Verified:\n  \
             GitHub -> Settings -> SSH and GPG keys -> New SSH key, Key type: Signing Key, then paste:\n  {public}\n  \
             (Or install gh, run `gh auth login`, and run `buzz paseo setup --replace-signing-key` for a new key.)"
        ));
    }
    added
}

/// `gh ssh-key add` with `public` on stdin, killed if it outlives `deadline`
/// (for example, waiting on authentication), so setup always continues.
fn run_gh_with_deadline(public: &str, deadline: std::time::Duration) -> bool {
    let Ok(mut child) = std::process::Command::new("gh")
        .args([
            "ssh-key",
            "add",
            "-",
            "--type",
            "signing",
            "--title",
            "Buzz agents",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    // Dropping stdin closes it, so gh reads the key and continues.
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(public.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// What the developer entered for one enroll request.
pub(super) struct Collected {
    /// The partial or complete profile.
    pub profile: Value,
    /// A new signing key's public half, to add to GitHub after enrolling.
    pub signing_public: Option<String>,
}

/// Collect a complete profile for a developer with nothing stored.
pub(super) async fn new_profile(
    prompt: &mut dyn Prompt,
    base: Map<String, Value>,
    local: &Local,
    github_api: &str,
    keygen: impl FnOnce(&str) -> Result<SigningKey, CliError>,
) -> Result<Collected, CliError> {
    prompt.note(
        "No agent profile is stored for you yet. Your remote agents and Paseo daemon use it.\n\
         Credentials are sent encrypted to the Sandbox manager only; nothing is saved locally.",
    );
    prompt.note(GITHUB_HELP);
    let token = prompt
        .secret("GitHub token")?
        .ok_or_else(|| CliError::Usage("a GitHub token is required".into()))?;
    let user = github_user(github_api, &token).await?;
    prompt.note(&format!("GitHub account: {}", user.login));
    let name = prompt.line(
        "Git name for agent commits",
        Some(user.name.as_deref().unwrap_or(&user.login)),
    )?;
    prompt.note("Git email: a verified email on your GitHub account.");
    let email = prompt.line("Git email", local.git_email.as_deref())?;
    if name.is_empty() || email.is_empty() {
        return Err(CliError::Usage("a Git name and email are required".into()));
    }
    prompt.note(OP_HELP);
    let op_token = prompt
        .secret("1Password service account token")?
        .ok_or_else(|| CliError::Usage("a 1Password service account token is required".into()))?;
    let models = model_credentials(prompt, false)?;
    let key = keygen(&user.login)?;
    let mut profile = base;
    profile.insert(
        "git".into(),
        json!({"name": name, "email": email, "ssh_signing_key": key.private}),
    );
    profile.insert(
        "github".into(),
        json!({"login": user.login, "token": token}),
    );
    profile.insert(
        "via".into(),
        json!({"profile": local.via_profile, "op_token": op_token}),
    );
    profile.insert("models".into(), Value::Object(models));
    Ok(Collected {
        profile: Value::Object(profile),
        signing_public: Some(key.public),
    })
}

/// Collect replacements for a stored profile; Enter keeps each value.
/// `local` supplies an explicitly named via profile; `keygen`, when given,
/// replaces the agents' signing key.
pub(super) async fn updated_profile(
    prompt: &mut dyn Prompt,
    base: Map<String, Value>,
    github_api: &str,
    local: &Local,
    keygen: Option<Keygen>,
) -> Result<Collected, CliError> {
    prompt.note("Press Enter to keep a stored value.");
    let mut profile = base;
    let mut signing_public = None;
    if let Some(keygen) = keygen {
        let developer = profile["developer"]
            .as_str()
            .unwrap_or("developer")
            .to_owned();
        let key = keygen(&developer)?;
        profile.insert("git".into(), json!({"ssh_signing_key": key.private}));
        signing_public = Some(key.public);
    }
    prompt.note(GITHUB_HELP);
    if let Some(token) = prompt.secret("New GitHub token")? {
        let user = github_user(github_api, &token).await?;
        profile.insert(
            "github".into(),
            json!({"login": user.login, "token": token}),
        );
    }
    prompt.note(OP_HELP);
    let mut via = Map::new();
    if let Some(op_token) = prompt.secret("New 1Password service account token")? {
        via.insert("op_token".into(), Value::String(op_token));
    }
    if local.via_explicit {
        via.insert("profile".into(), Value::String(local.via_profile.clone()));
    }
    if !via.is_empty() {
        profile.insert("via".into(), Value::Object(via));
    }
    let models = model_credentials(prompt, true)?;
    if !models.is_empty() {
        profile.insert("models".into(), Value::Object(models));
    }
    Ok(Collected {
        profile: Value::Object(profile),
        signing_public,
    })
}

/// Ask for a missing Claude subscription token, which the Paseo assistant needs.
/// `api_key` says the profile holds an Anthropic API key, which a
/// subscription token replaces for the agents too.
pub(super) fn assistant_token(
    prompt: &mut dyn Prompt,
    base: Map<String, Value>,
    api_key: bool,
) -> Result<Option<Collected>, CliError> {
    let (key, label, help) = MODELS[0];
    let replaces = if api_key {
        "\nEntering one replaces your Anthropic API key: your Claude Code agents then use the subscription too."
    } else {
        ""
    };
    prompt.note(&format!(
        "Your profile has no Claude subscription token, so your Paseo assistant cannot run.{replaces}\n{label}: {help}"
    ));
    let Some(token) = prompt.secret(&format!("{label} (Enter to skip)"))? else {
        return Ok(None);
    };
    let mut profile = base;
    profile.insert("models".into(), json!({ key: token }));
    Ok(Some(Collected {
        profile: Value::Object(profile),
        signing_public: None,
    }))
}

fn model_credentials(
    prompt: &mut dyn Prompt,
    update: bool,
) -> Result<Map<String, Value>, CliError> {
    prompt.note(if update {
        "Model credentials: Enter keeps the stored value. Setting one of a pair replaces the other."
    } else {
        "Model credentials: press Enter to skip any you do not use."
    });
    let mut models = Map::new();
    for (key, label, help) in MODELS {
        prompt.note(&format!("{label}: {help}"));
        if let Some(value) = prompt.secret(label)? {
            models.insert(key.into(), Value::String(value));
        }
    }
    if let Some((a, b)) = EXCLUSIVE
        .iter()
        .find(|(a, b)| models.contains_key(*a) && models.contains_key(*b))
    {
        return Err(CliError::Usage(format!("enter only one of {a} and {b}")));
    }
    Ok(models)
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
