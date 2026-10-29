//! Enrolling a developer profile with the Sandbox manager. The owner signs
//! a kind 24210 `enroll` request carrying the profile `profile.py`
//! validates, sends it on the community's live socket, and waits for the
//! `done` reply. Once a profile is on file, the request carries only what
//! changes, and the manager merges it; the `profile` request says what is on
//! file without any value. Tokens are never saved or logged, and leave the
//! form once it is sent.

use super::signing::{self, GitHubIdentity};
use buzz_identity::Identity;
use buzz_relay_client::Session;
use buzz_relay_client::socket::{MAX_PUBLICATION_FRAME, SubscriptionEvent};
use buzz_session_control::{KIND, Reply, Status};
use futures::{Stream, StreamExt};
use nostr::prelude::PublicKey;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// How long the manager has to answer.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// A form field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    GithubToken,
    GitEmail,
    ViaProfile,
    OpToken,
    CodexAccessToken,
    ClaudeOauthToken,
    OpenaiApiKey,
    AnthropicApiKey,
    OpenrouterApiKey,
}

impl Field {
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

    /// Masked, and cleared once the form is sent.
    pub fn secret(self) -> bool {
        matches!(
            self,
            Field::GithubToken
                | Field::OpToken
                | Field::CodexAccessToken
                | Field::OpenaiApiKey
                | Field::AnthropicApiKey
                | Field::ClaudeOauthToken
                | Field::OpenrouterApiKey
        )
    }

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

    /// The field filling the models entry `key`.
    pub fn of_model(key: &str) -> Option<Field> {
        Field::ALL
            .into_iter()
            .find(|field| field.model() == Some(key))
    }

    /// The models entry it fills, for the optional model keys.
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

/// The form's text, by field.
#[derive(Default)]
pub struct Values([Zeroizing<String>; Field::ALL.len()]);

impl Values {
    pub fn get(&self, field: Field) -> &str {
        &self.0[field as usize]
    }

    pub fn set(&mut self, field: Field, text: String) {
        self.0[field as usize] = Zeroizing::new(text);
    }

    /// What is sent: every field. The form keeps all but the secrets.
    pub fn submit(&mut self) -> Values {
        let mut sent = Values::default();
        for field in Field::ALL {
            let kept = if field.secret() {
                String::new()
            } else {
                self.get(field).to_owned()
            };
            sent.0[field as usize] =
                std::mem::replace(&mut self.0[field as usize], Zeroizing::new(kept));
        }
        sent
    }
}

/// What the manager holds for this developer: names and flags, never a
/// credential. The login, name and email are not secret.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Summary {
    pub version_id: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub git: GitSummary,
    #[serde(default)]
    pub github: GitHubSummary,
    #[serde(default)]
    pub via: ViaSummary,
    /// The stored models entries, by key.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub codex_login: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GitSummary {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub signing_key: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GitHubSummary {
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub token: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ViaSummary {
    #[serde(default)]
    pub profile: bool,
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

    /// The stored model credentials, as fields, in form order.
    pub fn model_fields(&self) -> Vec<Field> {
        Field::ALL
            .into_iter()
            .filter(|field| field.model().is_some() && self.holds(*field))
            .collect()
    }

    /// One line naming what is on file.
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
            self.model_fields()
                .iter()
                .map(|field| field.label().to_owned()),
        );
        if self.codex_login {
            parts.push("ChatGPT login".into());
        }
        let updated = self
            .updated_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| {
                format!(
                    " Updated {}.",
                    at.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M UTC")
                )
            })
            .unwrap_or_default();
        format!("On file: {}.{updated}", parts.join(" · "))
    }
}

/// What the form knows about the stored profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnFile {
    Loading,
    Nothing,
    Stored(Summary),
    /// The `profile` request failed, and why: every field is required.
    Unknown(String),
}

impl OnFile {
    pub fn summary(&self) -> Option<&Summary> {
        match self {
            OnFile::Stored(summary) => Some(summary),
            _ => None,
        }
    }

    pub fn text(&self) -> String {
        match self {
            OnFile::Loading => "Checking what is on file…".into(),
            OnFile::Nothing => "Nothing enrolled yet.".into(),
            OnFile::Stored(summary) => summary.text(),
            OnFile::Unknown(error) => {
                format!("Could not check what is on file: {error} Enroll the complete profile.")
            }
        }
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

/// The profile `profile.py` validates, for the owner `owner`, or why not.
/// `github` is the token's account, which names the Git author, and
/// `signing_key` the agent-only key commits are signed with. The developer
/// ID is left for the Sandbox manager, which knows it from the owner key.
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

/// What an enrollment over a stored profile changes, beyond the form's text.
#[derive(Default)]
pub struct Changes<'a> {
    /// The new GitHub token's account; `None` keeps the stored token.
    pub github: Option<&'a GitHubIdentity>,
    /// A new signing key; `None` keeps the stored one.
    pub signing_key: Option<&'a str>,
    /// Stored model credentials to remove, by models key.
    pub removed: &'a [&'static str],
}

/// The partial profile the manager merges into the stored one: only the
/// fields filled in, `null` for each removed model credential, and nothing
/// for what stays. Each field sent is checked as `profile.py` checks it.
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
    let mut models: Map<String, Value> = changes
        .removed
        .iter()
        .map(|key| ((*key).to_owned(), Value::Null))
        .collect();
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
        return Err("Nothing to change: fill in a field, or remove a credential.".into());
    }
    validate_partial(&profile)?;
    Ok(profile)
}

/// `profile.py`'s size limit, which Secrets Manager sets.
const MAX_PROFILE_BYTES: usize = 65_536;

/// `profile.py`'s `validate`, with its messages, which name no values.
pub fn validate(profile: &Value) -> Result<(), String> {
    fields(
        profile,
        &[
            "version",
            "developer",
            "owner_pubkey",
            "git",
            "github",
            "via",
            "models",
        ],
    )?;
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

/// [`validate`] for a partial profile: its fields are those of a full one,
/// each optional, and each present one passes the same rule. A `null`
/// model credential is a removal.
pub fn validate_partial(profile: &Value) -> Result<(), String> {
    let only = |value: &Value, allowed: &[&str]| {
        let ok = value
            .as_object()
            .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())));
        if ok {
            Ok(())
        } else {
            Err("Profile fields are incomplete or unsupported".to_string())
        }
    };
    only(
        profile,
        &[
            "version",
            "developer",
            "owner_pubkey",
            "git",
            "github",
            "via",
            "models",
        ],
    )?;
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
        let set: Map<String, Value> = stored
            .as_object()
            .ok_or("Unsupported model credential type")?
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let model_fields = [
            "openai_api_key",
            "codex_access_token",
            "anthropic_api_key",
            "claude_oauth_token",
            "openrouter_api_key",
        ];
        let removals_known = stored.as_object().is_some_and(|stored| {
            stored
                .keys()
                .all(|key| model_fields.contains(&key.as_str()))
        });
        if !removals_known {
            return Err("Unsupported model credential type".into());
        }
        models(&Value::Object(set))?;
    }
    size(profile)
}

fn require(ok: bool, message: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message.to_owned()) }
}

fn matches(value: &Value, pattern: &str) -> bool {
    let pattern = Regex::new(&format!("^(?:{pattern})$")).expect("a valid pattern");
    value.as_str().is_some_and(|text| pattern.is_match(text))
}

fn owner_fields(profile: &Value) -> Result<(), String> {
    require(
        profile["version"] == json!(1),
        "Unsupported profile version",
    )?;
    // Empty until the manager fills in the signing owner's developer ID.
    let developer = profile.get("developer").unwrap_or(&Value::Null);
    require(
        developer.is_null()
            || *developer == json!("")
            || matches(developer, "[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?"),
        "Invalid developer ID",
    )?;
    require(
        matches(&profile["owner_pubkey"], "[0-9a-f]{64}"),
        "Invalid Buzz owner",
    )
}

fn git_identity(value: &Value) -> Result<(), String> {
    let ok = value.as_str().is_some_and(|text| {
        (1..=256).contains(&text.chars().count()) && !text.chars().any(|c| (c as u32) < 32)
    });
    require(ok, "Invalid Git identity")
}

fn signing_key(value: &Value) -> Result<(), String> {
    require(
        matches(
            value,
            "-----BEGIN OPENSSH PRIVATE KEY-----\\n[A-Za-z0-9+/=\\n]{1,4000}-----END OPENSSH PRIVATE KEY-----\\n?",
        ),
        "Invalid SSH signing key",
    )
}

fn github_account(github: &Value) -> Result<(), String> {
    require(
        matches(&github["login"], "[a-zA-Z0-9][a-zA-Z0-9-]{0,38}"),
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

fn models(models: &Value) -> Result<(), String> {
    let models = models
        .as_object()
        .ok_or("Unsupported model credential type")?;
    let model_fields = [
        "openai_api_key",
        "codex_access_token",
        "anthropic_api_key",
        "claude_oauth_token",
        "openrouter_api_key",
    ];
    require(
        models
            .keys()
            .all(|key| model_fields.contains(&key.as_str())),
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

/// `fields`: an object with exactly `expected`.
fn fields(value: &Value, expected: &[&str]) -> Result<(), String> {
    let ok = value.as_object().is_some_and(|object| {
        object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
    });
    if ok {
        Ok(())
    } else {
        Err("Profile fields are incomplete or unsupported".into())
    }
}

/// `token`: 1 to 8192 characters, no whitespace.
fn token(value: &Value) -> Result<(), String> {
    let ok = value.as_str().is_some_and(|text| {
        (1..=8192).contains(&text.chars().count()) && !text.chars().any(char::is_whitespace)
    });
    if ok {
        Ok(())
    } else {
        Err("Invalid credential".into())
    }
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
    /// The manager stored a new version of the profile, which now holds
    /// `summary` when the manager says.
    Enrolled {
        version_id: String,
        summary: Option<Summary>,
    },
    /// The manager refused it with `code`.
    Refused { code: String },
}

/// The manager's refusal codes, and what each asks of the developer.
pub const REFUSALS: &[(&str, &str)] = &[
    (
        "unauthorized_owner",
        "This Buzz identity is not a configured developer.",
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
        "The Sandbox manager could not store the profile. Try again.",
    ),
];

impl Outcome {
    /// What the form shows.
    pub fn text(&self) -> String {
        match self {
            Outcome::Enrolled { version_id, .. } => {
                format!("Enrolled. Profile version {version_id}.")
            }
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

/// What a `done` reply's response says. Only the code of a refusal is
/// read: its message is the manager's.
pub fn outcome(response: &Value) -> Result<Outcome, String> {
    let text = |name: &str| {
        response
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => text("version_id").map(|version_id| Outcome::Enrolled {
            version_id,
            // A summary the form cannot read leaves it to ask again.
            summary: response
                .get("profile")
                .and_then(|summary| serde_json::from_value(summary.clone()).ok()),
        }),
        Some(false) => text("code").map(|code| Outcome::Refused { code }),
        None => None,
    }
    .ok_or_else(|| "The Sandbox manager's reply was malformed.".into())
}

/// The response in the `done` reply to `request_id` among `replies`,
/// skipping `accepted` and `progress` and whatever `open` refuses.
pub async fn done<T>(
    mut replies: impl Stream<Item = T> + Unpin,
    request_id: &str,
    open: impl Fn(&T) -> Option<Reply>,
) -> Result<Value, String> {
    while let Some(item) = replies.next().await {
        let Some(reply) = open(&item) else {
            continue;
        };
        if reply.request_id == request_id && reply.status == Status::Done {
            return Ok(reply.response.unwrap_or(Value::Null));
        }
    }
    Err("The community connection closed before the Sandbox manager answered.".into())
}

/// Model credentials to remove or replace in one enrollment.
pub struct ModelChanges {
    /// Stored model fields to remove, including `codex_login` to disconnect it.
    pub removed: Vec<&'static str>,
    /// Dedicated native ChatGPT cache, encrypted only to the manager.
    pub codex_login: Option<Zeroizing<String>>,
}

/// Enrolls the form's `values` as `identity`.
///
/// With nothing on file, it reads the GitHub token's account, creates the
/// agent-only signing key, and sends the complete profile. With a profile on
/// file, it sends only what changes: a blank GitHub token keeps the stored
/// one, and the stored signing key stays unless `replace_key`. `removed`
/// names stored model credentials to drop. Once the manager stores a new
/// signing key, its public half goes to the GitHub account with the local
/// `gh`, or back to the developer to add; the token needs no account
/// permission either way.
pub async fn run(
    session: Arc<Session>,
    identity: Arc<Identity>,
    manager: String,
    mut values: Values,
    on_file: Option<Summary>,
    replace_key: bool,
    models: ModelChanges,
) -> Result<(Outcome, Option<signing::Registration>), String> {
    let ModelChanges {
        removed,
        codex_login,
    } = models;
    if codex_login.is_some() {
        values.set(Field::OpenaiApiKey, String::new());
        values.set(Field::CodexAccessToken, String::new());
    }
    let chatgpt_login = match codex_login {
        Some(cache) => Some(
            serde_json::from_str::<Value>(&cache)
                .map_err(|_| "The dedicated ChatGPT login is invalid.".to_string())?,
        ),
        None if removed.contains(&"codex_login") => Some(Value::Null),
        None => None,
    };
    let model_removals: Vec<_> = removed
        .into_iter()
        .filter(|key| *key != "codex_login")
        .collect();
    let token = values.get(Field::GithubToken).trim().to_owned();
    let github = if !token.is_empty() {
        Some(signing::identity(&token).await?)
    } else if on_file.as_ref().is_some_and(|summary| summary.github.token) {
        None
    } else {
        return Err("Enter a GitHub token.".into());
    };
    let login = github
        .as_ref()
        .map(|github| github.login.clone())
        .or_else(|| on_file.as_ref().map(|summary| summary.github.login.clone()))
        .filter(|login| !login.is_empty())
        .ok_or("Enter a GitHub token.")?;
    let new_key = replace_key
        || !on_file
            .as_ref()
            .is_some_and(|summary| summary.git.signing_key);
    let key = if new_key {
        Some(signing::generate(&format!("buzz agents {login}")).await?)
    } else {
        None
    };
    let owner = identity.pubkey_hex();
    let profile = match (&on_file, &github, &key) {
        (None, Some(github), Some(key)) => profile(&values, &owner, github, &key.private)?,
        (None, _, _) => return Err("Enter a GitHub token.".into()),
        (Some(_), _, _) => partial(
            &values,
            &owner,
            Changes {
                github: github.as_ref(),
                signing_key: key.as_ref().map(|key| key.private.as_str()),
                removed: &model_removals,
            },
        )?,
    };
    let outcome = match chatgpt_login {
        Some(login) => enroll_with_login(session, identity, manager, profile, Some(login)).await?,
        None => enroll(session, identity, manager, profile).await?,
    };
    let (Outcome::Enrolled { .. }, Some(key)) = (&outcome, key) else {
        return Ok((outcome, None));
    };
    let title = format!(
        "Buzz agents ({})",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );
    let registration = signing::register(&login, &title, &key.public).await;
    Ok((outcome, Some(registration)))
}

/// Signs the `enroll` request for `profile` to `manager` as `identity`,
/// sends it on `session`'s live socket, and waits up to [`TIMEOUT`] for
/// its `done` reply.
pub async fn enroll(
    session: Arc<Session>,
    identity: Arc<Identity>,
    manager: String,
    profile: Value,
) -> Result<Outcome, String> {
    enroll_with_login(session, identity, manager, profile, None).await
}

async fn enroll_with_login(
    session: Arc<Session>,
    identity: Arc<Identity>,
    manager: String,
    profile: Value,
    login: Option<Value>,
) -> Result<Outcome, String> {
    let manager = PublicKey::from_hex(&manager).map_err(|_| "The manager key is not valid.")?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = identity
        .enroll_request_with_codex_login(&manager, &request_id, profile, login, now())
        .map_err(|error| format!("Could not sign the enrollment ({}).", error.label()))?;
    if json!(["EVENT", request]).to_string().len() > MAX_PUBLICATION_FRAME {
        return Err("The profile is too large to send. Shorten the via profile.".into());
    }
    let response = ask(&session, &identity, &manager, request, &request_id).await?;
    outcome(&response)
}

/// What the manager has on file for `identity`'s developer, by the
/// `profile` request: `None` when nothing is stored.
pub async fn on_file(
    session: Arc<Session>,
    identity: Arc<Identity>,
    manager: String,
) -> Result<Option<Summary>, String> {
    let manager = PublicKey::from_hex(&manager).map_err(|_| "The manager key is not valid.")?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = identity
        .profile_request(&manager, &request_id, now())
        .map_err(|error| format!("Could not sign the request ({}).", error.label()))?;
    let response = ask(&session, &identity, &manager, request, &request_id).await?;
    on_file_reply(&response)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Sends `request` on `session`'s live socket and waits up to [`TIMEOUT`]
/// for the manager's `done` response to it.
async fn ask(
    session: &Session,
    identity: &Identity,
    manager: &PublicKey,
    request: nostr::prelude::Event,
    request_id: &str,
) -> Result<Value, String> {
    let filter = json!({
        "kinds": [KIND], "#p": [identity.pubkey_hex()], "#e": [request.id.to_hex()],
    });
    let Value::Object(filter) = filter else {
        unreachable!("an object");
    };
    let exchange = async {
        let (mut replies, publisher) = session.live_exchange(vec![filter]).await?;
        // Replies are ephemeral: listen before sending.
        loop {
            match replies.next().await {
                Some(SubscriptionEvent::Eose) => break,
                Some(SubscriptionEvent::Closed(_)) | None => {
                    return Err("The community would not deliver the reply.".to_string());
                }
                Some(_) => {}
            }
        }
        publisher
            .publish(request.clone())
            .await
            .map_err(|error| error.to_string())?;
        let events = replies.filter_map(|item| {
            futures::future::ready(match item {
                SubscriptionEvent::Event(verified) => Some(verified.event().clone()),
                _ => None,
            })
        });
        done(Box::pin(events), request_id, |event| {
            identity
                .open_session_reply(event, manager, &request.id)
                .ok()
        })
        .await
    };
    tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| "The Sandbox manager did not answer within 60 seconds.".to_string())?
}

#[cfg(test)]
#[path = "enroll_tests.rs"]
mod tests;
