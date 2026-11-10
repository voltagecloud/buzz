//! The developer profile `buzz onboard` enrolls, built and checked exactly as
//! the GPUI enroll form builds and checks it
//! (`apps/buzz-gpui/crates/app/src/agent_plugins/remote_workspace/enroll.rs`),
//! which mirrors the agent image's `profile.py`. Messages never name a value.

use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

/// A credential or setting the profile carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Fine-grained GitHub token for clone, push and PRs.
    GithubToken,
    /// A verified email on the GitHub account.
    GitEmail,
    /// The via profile TOML (only `op://` references).
    ViaProfile,
    /// 1Password service account token for the Buzz Agents Dev vault.
    OpToken,
    /// ChatGPT Business or Enterprise access token (`at-…`).
    CodexAccessToken,
    /// Claude subscription token from `claude setup-token`.
    ClaudeOauthToken,
    /// OpenAI API key.
    OpenaiApiKey,
    /// Anthropic API key.
    AnthropicApiKey,
    /// OpenRouter API key.
    OpenrouterApiKey,
}

impl Field {
    /// Every field, in the GPUI form's order.
    pub const ALL: [Field; 9] = [
        Field::GithubToken,
        Field::GitEmail,
        Field::ViaProfile,
        Field::OpToken,
        Field::CodexAccessToken,
        Field::ClaudeOauthToken,
        Field::OpenaiApiKey,
        Field::AnthropicApiKey,
        Field::OpenrouterApiKey,
    ];

    /// What the developer sees.
    pub fn label(self) -> &'static str {
        match self {
            Field::GithubToken => "GitHub token",
            Field::GitEmail => "Git email",
            Field::ViaProfile => "via profile",
            Field::OpToken => "1Password service account token",
            Field::CodexAccessToken => "Codex access token",
            Field::OpenaiApiKey => "OpenAI API key",
            Field::AnthropicApiKey => "Anthropic API key",
            Field::ClaudeOauthToken => "Claude subscription token",
            Field::OpenrouterApiKey => "OpenRouter API key",
        }
    }

    /// The `models` entry it fills, for the model credentials.
    pub fn model(self) -> Option<&'static str> {
        match self {
            Field::OpenaiApiKey => Some("openai_api_key"),
            Field::CodexAccessToken => Some("codex_access_token"),
            Field::AnthropicApiKey => Some("anthropic_api_key"),
            Field::ClaudeOauthToken => Some("claude_oauth_token"),
            Field::OpenrouterApiKey => Some("openrouter_api_key"),
            _ => None,
        }
    }
}

/// The collected values, by field. Wiped on drop.
#[derive(Default)]
pub struct Values([Zeroizing<String>; Field::ALL.len()]);

impl Values {
    /// The text for `field`; empty when not collected.
    pub fn get(&self, field: Field) -> &str {
        &self.0[field as usize]
    }

    /// Replaces the text for `field`.
    pub fn set(&mut self, field: Field, text: String) {
        self.0[field as usize] = Zeroizing::new(text);
    }

    /// Whether `field` holds non-blank text.
    pub fn filled(&self, field: Field) -> bool {
        !self.get(field).trim().is_empty()
    }
}

/// Who a GitHub token belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubIdentity {
    /// The account login.
    pub login: String,
    /// The profile name, or the login when the account has none.
    pub name: String,
}

/// What the manager holds for this developer: names and flags, never a
/// credential.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Summary {
    /// The stored profile version.
    pub version_id: String,
    /// When the profile was last stored (RFC 3339).
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Git identity and signing key presence.
    #[serde(default)]
    pub git: GitSummary,
    /// GitHub login and token presence.
    #[serde(default)]
    pub github: GitHubSummary,
    /// via profile and 1Password token presence.
    #[serde(default)]
    pub via: ViaSummary,
    /// The stored `models` entries, by key.
    #[serde(default)]
    pub models: Vec<String>,
    /// Whether a dedicated ChatGPT login is stored.
    #[serde(default)]
    pub codex_login: bool,
}

/// The profile's Git section, without the key.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GitSummary {
    /// Git author name.
    #[serde(default)]
    pub name: String,
    /// Git author email.
    #[serde(default)]
    pub email: String,
    /// Whether an agent signing key is stored.
    #[serde(default)]
    pub signing_key: bool,
}

/// The profile's GitHub section, without the token.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GitHubSummary {
    /// The token's login.
    #[serde(default)]
    pub login: String,
    /// Whether a token is stored.
    #[serde(default)]
    pub token: bool,
}

/// The profile's via section, without values.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ViaSummary {
    /// Whether a via profile is stored.
    #[serde(default)]
    pub profile: bool,
    /// Whether a 1Password token is stored.
    #[serde(default)]
    pub op_token: bool,
}

impl Summary {
    /// Whether `field` has a stored value.
    pub fn holds(&self, field: Field) -> bool {
        match field {
            Field::GithubToken => self.github.token,
            Field::GitEmail => !self.git.email.is_empty(),
            Field::ViaProfile => self.via.profile,
            Field::OpToken => self.via.op_token,
            _ => field
                .model()
                .is_some_and(|key| self.models.iter().any(|stored| stored == key)),
        }
    }

    /// One line naming what is on file, never a value.
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        if !self.github.login.is_empty() {
            parts.push(format!("GitHub @{}", self.github.login));
        }
        if !self.git.email.is_empty() {
            parts.push(format!("Git email {}", self.git.email));
        }
        for (held, name) in [
            (self.git.signing_key, "signing key"),
            (self.via.profile, "via profile"),
            (self.via.op_token, "1Password token"),
        ] {
            if held {
                parts.push(name.to_owned());
            }
        }
        parts.extend(
            Field::ALL
                .into_iter()
                .filter(|field| field.model().is_some() && self.holds(*field))
                .map(|field| field.label().to_owned()),
        );
        if self.codex_login {
            parts.push("ChatGPT login".into());
        }
        format!("On file: {}.", parts.join(" · "))
    }
}

/// What the `profile` request's `done` response says: the summary, `None`
/// when nothing is stored, or why not.
pub fn on_file_reply(response: &Value) -> Result<Option<Summary>, String> {
    let malformed = || "The Sandbox manager's reply was malformed.".to_string();
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => match response.get("profile") {
            None | Some(Value::Null) => Ok(None),
            Some(summary) => serde_json::from_value(summary.clone())
                .map(Some)
                .map_err(|_| malformed()),
        },
        Some(false) => Err(outcome(response)?.text()),
        None => Err(malformed()),
    }
}

/// The complete profile for the owner `owner`, or why not. `github` is the
/// token's account and `signing_key` the agent-only key. The developer ID is
/// left for the Sandbox manager, which knows it from the owner key.
pub fn profile(
    values: &Values,
    owner: &str,
    github: &GitHubIdentity,
    signing_key: &str,
) -> Result<Value, String> {
    let text = |field| values.get(field).trim();
    let models: Map<String, Value> = Field::ALL
        .into_iter()
        .filter_map(|field| Some((field.model()?, text(field))))
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.into()))
        .collect();
    let profile = json!({
        "version": 1,
        "developer": "",
        "owner_pubkey": owner,
        "git": {
            "name": github.name,
            "email": text(Field::GitEmail),
            "ssh_signing_key": signing_key,
        },
        "github": {"login": github.login, "token": text(Field::GithubToken)},
        "via": {"profile": values.get(Field::ViaProfile), "op_token": text(Field::OpToken)},
        "models": models,
    });
    validate(&profile)?;
    Ok(profile)
}

/// What an enrollment over a stored profile changes, beyond the values.
#[derive(Default)]
pub struct Changes<'a> {
    /// The new GitHub token's account; `None` keeps the stored token.
    pub github: Option<&'a GitHubIdentity>,
    /// A new signing key; `None` keeps the stored one.
    pub signing_key: Option<&'a str>,
}

/// The partial profile the manager merges into the stored one: only the
/// fields filled in. Each field sent is checked as a full profile's is.
pub fn partial(values: &Values, owner: &str, changes: Changes) -> Result<Value, String> {
    let text = |field| values.get(field).trim();
    let mut git = Map::new();
    if let Some(github) = changes.github {
        git.insert("name".into(), github.name.clone().into());
    }
    if !text(Field::GitEmail).is_empty() {
        git.insert("email".into(), text(Field::GitEmail).into());
    }
    if let Some(key) = changes.signing_key {
        git.insert("ssh_signing_key".into(), key.into());
    }
    let mut via = Map::new();
    if !text(Field::ViaProfile).is_empty() {
        via.insert("profile".into(), values.get(Field::ViaProfile).into());
    }
    if !text(Field::OpToken).is_empty() {
        via.insert("op_token".into(), text(Field::OpToken).into());
    }
    let mut models = Map::new();
    for field in Field::ALL {
        if let Some(key) = field.model().filter(|_| !text(field).is_empty()) {
            models.insert(key.to_owned(), text(field).into());
        }
    }
    let mut profile = json!({"version": 1, "developer": "", "owner_pubkey": owner});
    for (name, part) in [("git", git), ("via", via), ("models", models)] {
        if !part.is_empty() {
            profile[name] = Value::Object(part);
        }
    }
    if let Some(github) = changes.github {
        profile["github"] = json!({"login": github.login, "token": text(Field::GithubToken)});
    }
    if profile
        .as_object()
        .is_some_and(|profile| profile.len() == 3)
    {
        return Err("Nothing to change: enter a credential to replace.".into());
    }
    validate_partial(&profile)?;
    Ok(profile)
}

/// `profile.py`'s size limit, which Secrets Manager sets.
const MAX_PROFILE_BYTES: usize = 65_536;

const PROFILE_FIELDS: [&str; 7] = [
    "version",
    "developer",
    "owner_pubkey",
    "git",
    "github",
    "via",
    "models",
];

const MODEL_FIELDS: [&str; 5] = [
    "openai_api_key",
    "codex_access_token",
    "anthropic_api_key",
    "claude_oauth_token",
    "openrouter_api_key",
];

/// `profile.py`'s `validate`, with its messages, which name no values.
pub fn validate(profile: &Value) -> Result<(), String> {
    fields(profile, &PROFILE_FIELDS)?;
    owner_fields(profile)?;
    let git = &profile["git"];
    fields(git, &["name", "email", "ssh_signing_key"])?;
    git_identity(&git["name"])?;
    git_identity(&git["email"])?;
    signing_key(&git["ssh_signing_key"])?;
    let github = &profile["github"];
    fields(github, &["login", "token"])?;
    github_account(github)?;
    let via = &profile["via"];
    fields(via, &["profile", "op_token"])?;
    token(&via["op_token"])?;
    via_profile(&via["profile"])?;
    models(&profile["models"])?;
    size(profile)
}

/// [`validate`] for a partial profile: each present field passes the same
/// rule as in a full one.
pub fn validate_partial(profile: &Value) -> Result<(), String> {
    let only = |value: &Value, allowed: &[&str]| {
        let ok = value
            .as_object()
            .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())));
        require(ok, "Profile fields are incomplete or unsupported")
    };
    only(profile, &PROFILE_FIELDS)?;
    owner_fields(profile)?;
    if let Some(git) = profile.get("git") {
        only(git, &["name", "email", "ssh_signing_key"])?;
        for name in ["name", "email"] {
            git.get(name).map_or(Ok(()), git_identity)?;
        }
        git.get("ssh_signing_key").map_or(Ok(()), signing_key)?;
    }
    if let Some(github) = profile.get("github") {
        fields(github, &["login", "token"])?;
        github_account(github)?;
    }
    if let Some(via) = profile.get("via") {
        only(via, &["profile", "op_token"])?;
        via.get("op_token").map_or(Ok(()), token)?;
        via.get("profile").map_or(Ok(()), via_profile)?;
    }
    if let Some(stored) = profile.get("models") {
        models(stored)?;
    }
    size(profile)
}

fn require(ok: bool, message: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn lowercase_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn owner_fields(profile: &Value) -> Result<(), String> {
    require(
        profile["version"] == json!(1),
        "Unsupported profile version",
    )?;
    let developer = profile.get("developer").unwrap_or(&Value::Null);
    require(
        developer.is_null() || developer.as_str().is_some_and(developer_id),
        "Invalid developer ID",
    )?;
    require(
        profile["owner_pubkey"]
            .as_str()
            .is_some_and(|owner| lowercase_hex(owner, 64)),
        "Invalid Buzz owner",
    )
}

/// `""`, or `[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?`.
fn developer_id(text: &str) -> bool {
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = text.as_bytes();
    text.is_empty()
        || (bytes.len() <= 32
            && bytes.first().copied().is_some_and(edge)
            && bytes.last().copied().is_some_and(edge)
            && bytes.iter().all(|b| edge(*b) || *b == b'-'))
}

fn git_identity(value: &Value) -> Result<(), String> {
    let ok = value.as_str().is_some_and(|text| {
        (1..=256).contains(&text.chars().count()) && !text.chars().any(|c| (c as u32) < 32)
    });
    require(ok, "Invalid Git identity")
}

/// `-----BEGIN OPENSSH PRIVATE KEY-----\n[A-Za-z0-9+/=\n]{1,4000}-----END
/// OPENSSH PRIVATE KEY-----\n?`.
fn signing_key(value: &Value) -> Result<(), String> {
    const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n";
    const END: &str = "-----END OPENSSH PRIVATE KEY-----";
    let ok = value.as_str().is_some_and(|text| {
        let text = text.strip_suffix('\n').unwrap_or(text);
        text.strip_prefix(BEGIN)
            .and_then(|rest| rest.strip_suffix(END))
            .is_some_and(|body| {
                (1..=4000).contains(&body.len())
                    && body.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n')
                    })
            })
    });
    require(ok, "Invalid SSH signing key")
}

/// `[a-zA-Z0-9][a-zA-Z0-9-]{0,38}`.
fn github_login(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=39).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

fn github_account(github: &Value) -> Result<(), String> {
    require(
        github["login"].as_str().is_some_and(github_login),
        "Invalid GitHub login",
    )?;
    token(&github["token"])
}

fn via_profile(value: &Value) -> Result<(), String> {
    let config = value.as_str().ok_or("Invalid via profile")?;
    // The parser's error quotes the profile, so it is not shown.
    let config: toml::Table = config.parse().map_err(|_| "Invalid via profile")?;
    require(
        truthy(config.get("services")) && truthy(config.get("providers")),
        "via profile requires providers and services",
    )
}

/// The `models` rules, including the two mutual exclusions.
pub fn models(models: &Value) -> Result<(), String> {
    let models = models
        .as_object()
        .ok_or("Unsupported model credential type")?;
    require(
        models
            .keys()
            .all(|key| MODEL_FIELDS.contains(&key.as_str())),
        "Unsupported model credential type",
    )?;
    // Codex reads the access token before its saved API-key login, so allow only one.
    require(
        !(models.contains_key("openai_api_key") && models.contains_key("codex_access_token")),
        "Choose either an OpenAI API key or a Codex access token",
    )?;
    // Claude Code prefers an API key over the subscription token, so allow only one.
    require(
        !(models.contains_key("anthropic_api_key") && models.contains_key("claude_oauth_token")),
        "Choose either a Claude API key or a Claude subscription token",
    )?;
    for value in models.values() {
        token(value)?;
    }
    require(
        models
            .get("codex_access_token")
            .is_none_or(|value| value.as_str().is_some_and(|value| value.starts_with("at-"))),
        "Use a Codex access token (at-...)",
    )
}

fn size(profile: &Value) -> Result<(), String> {
    require(
        profile.to_string().len() <= MAX_PROFILE_BYTES,
        "Profile exceeds Secrets Manager size limit",
    )
}

/// An object with exactly `expected`.
fn fields(value: &Value, expected: &[&str]) -> Result<(), String> {
    let ok = value.as_object().is_some_and(|object| {
        object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
    });
    require(ok, "Profile fields are incomplete or unsupported")
}

/// 1 to 8192 characters, no whitespace.
fn token(value: &Value) -> Result<(), String> {
    let ok = value.as_str().is_some_and(|text| {
        (1..=8192).contains(&text.chars().count()) && !text.chars().any(char::is_whitespace)
    });
    require(ok, "Invalid credential")
}

/// Python's truthiness, for the via profile's tables.
fn truthy(value: Option<&toml::Value>) -> bool {
    match value {
        None => false,
        Some(toml::Value::String(text)) => !text.is_empty(),
        Some(toml::Value::Integer(n)) => *n != 0,
        Some(toml::Value::Float(n)) => *n != 0.0,
        Some(toml::Value::Boolean(b)) => *b,
        Some(toml::Value::Datetime(_)) => true,
        Some(toml::Value::Array(items)) => !items.is_empty(),
        Some(toml::Value::Table(table)) => !table.is_empty(),
    }
}

/// How an enrollment ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The manager stored a new profile version.
    Enrolled {
        /// The stored version.
        version_id: String,
    },
    /// The manager refused it with `code`.
    Refused {
        /// The manager's refusal code.
        code: String,
    },
}

/// The manager's refusal codes, and what each asks of the developer.
pub const REFUSALS: &[(&str, &str)] = &[
    (
        "unauthorized_owner",
        "This Buzz identity is not a configured developer. Ask an admin to add your owner key \
         (see voltage/remote-agents.md, Onboard a developer), then run buzz onboard again.",
    ),
    (
        "profile_owner_mismatch",
        "The profile's owner is not this Buzz identity.",
    ),
    (
        "profile_developer_mismatch",
        "The developer ID is not the one configured for this identity.",
    ),
    (
        "invalid_profile",
        "The Sandbox manager found the profile invalid.",
    ),
    (
        "github_login_mismatch",
        "The GitHub token belongs to a different login.",
    ),
    (
        "profile_store_failed",
        "The Sandbox manager could not store the profile. Run buzz onboard again.",
    ),
    (
        "paseo_store_failed",
        "The profile was stored, but not your Paseo daemon's secret. Run buzz onboard --reenroll.",
    ),
];

impl Outcome {
    /// What the developer sees.
    pub fn text(&self) -> String {
        match self {
            Outcome::Enrolled { version_id } => format!("Enrolled. Profile version {version_id}."),
            Outcome::Refused { code } => {
                let reason = REFUSALS
                    .iter()
                    .find(|(known, _)| known == code)
                    .map_or("", |(_, reason)| reason);
                format!("Refused ({code}). {reason}").trim_end().to_owned()
            }
        }
    }
}

/// What a `done` reply's response says. Only the code of a refusal is read:
/// its message is the manager's.
pub fn outcome(response: &Value) -> Result<Outcome, String> {
    let text = |name: &str| {
        response
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => text("version_id").map(|version_id| Outcome::Enrolled { version_id }),
        Some(false) => text("code").map(|code| Outcome::Refused { code }),
        None => None,
    }
    .ok_or_else(|| "The Sandbox manager's reply was malformed.".into())
}
