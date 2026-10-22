//! Enrolling a developer profile with the Sandbox manager. The owner signs
//! a kind 24210 `enroll` request carrying the profile `profile.py`
//! validates, sends it on the community's live socket, and waits for the
//! `done` reply. Tokens are never saved or logged, and leave the form once
//! it is sent.

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
    Developer,
    GitName,
    GitEmail,
    SigningKey,
    GithubLogin,
    GithubToken,
    ViaProfile,
    OpToken,
    CodexAccessToken,
    ClaudeOauthToken,
    OpenaiApiKey,
    AnthropicApiKey,
    OpenrouterApiKey,
}

impl Field {
    pub const ALL: [Field; 13] = [
        Field::Developer,
        Field::GitName,
        Field::GitEmail,
        Field::SigningKey,
        Field::GithubLogin,
        Field::GithubToken,
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
            Field::Developer => "Developer ID",
            Field::GitName => "Git name",
            Field::GitEmail => "Git email",
            Field::SigningKey => "Signing key fingerprint",
            Field::GithubLogin => "GitHub login",
            Field::GithubToken => "GitHub token",
            Field::ViaProfile => "via profile",
            Field::OpToken => "1Password service account token",
            Field::CodexAccessToken => "Codex access token",
            Field::OpenaiApiKey => "OpenAI API key",
            Field::AnthropicApiKey => "Anthropic API key",
            Field::ClaudeOauthToken => "Claude subscription token",
            Field::OpenrouterApiKey => "OpenRouter API key",
        }
    }

    /// The models entry it fills, for the optional model keys.
    fn model(self) -> Option<&'static str> {
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

/// The profile `profile.py` validates, for the owner `owner`, or why not.
pub fn profile(values: &Values, owner: &str) -> Result<Value, String> {
    let text = |field| values.get(field).trim();
    let models: Map<String, Value> = Field::ALL
        .into_iter()
        .filter_map(|field| Some((field.model()?, text(field))))
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.into()))
        .collect();
    let profile = json!({
        "version": 1,
        "developer": text(Field::Developer),
        "owner_pubkey": owner,
        "git": {
            "name": text(Field::GitName),
            "email": text(Field::GitEmail),
            "signing_key": text(Field::SigningKey),
        },
        "github": {"login": text(Field::GithubLogin), "token": text(Field::GithubToken)},
        "via": {"profile": values.get(Field::ViaProfile), "op_token": text(Field::OpToken)},
        "models": models,
    });
    validate(&profile)?;
    Ok(profile)
}

/// `profile.py`'s size limit, which Secrets Manager sets.
const MAX_PROFILE_BYTES: usize = 65_536;

/// `profile.py`'s `validate`, with its messages, which name no values.
pub fn validate(profile: &Value) -> Result<(), String> {
    let require = |ok: bool, message: &str| if ok { Ok(()) } else { Err(message.to_owned()) };
    let matches = |value: &Value, pattern: &str| {
        let pattern = Regex::new(&format!("^(?:{pattern})$")).expect("a valid pattern");
        value.as_str().is_some_and(|text| pattern.is_match(text))
    };
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
    require(
        profile["version"] == json!(1),
        "Unsupported profile version",
    )?;
    require(
        matches(
            &profile["developer"],
            "[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?",
        ),
        "Invalid developer ID",
    )?;
    require(
        matches(&profile["owner_pubkey"], "[0-9a-f]{64}"),
        "Invalid Buzz owner",
    )?;
    let git = &profile["git"];
    fields(git, &["name", "email", "signing_key"])?;
    for value in git.as_object().into_iter().flat_map(|git| git.values()) {
        let ok = value.as_str().is_some_and(|text| {
            (1..=256).contains(&text.chars().count()) && !text.chars().any(|c| (c as u32) < 32)
        });
        require(ok, "Invalid Git identity")?;
    }
    require(
        matches(&git["signing_key"], "[0-9A-Fa-f]{40,64}"),
        "Use a full OpenPGP signing fingerprint",
    )?;
    let github = &profile["github"];
    fields(github, &["login", "token"])?;
    require(
        matches(&github["login"], "[a-zA-Z0-9][a-zA-Z0-9-]{0,38}"),
        "Invalid GitHub login",
    )?;
    token(&github["token"])?;
    let via = &profile["via"];
    fields(via, &["profile", "op_token"])?;
    token(&via["op_token"])?;
    let config = via["profile"].as_str().ok_or("Invalid via profile")?;
    // The parser's error quotes the profile, so it is not shown.
    let config: toml::Table = config.parse().map_err(|_| "Invalid via profile")?;
    require(
        truthy(config.get("services")) && truthy(config.get("providers")),
        "via profile requires providers and services",
    )?;
    let models = profile["models"]
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
    )?;
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
    /// The manager stored a new version of the profile.
    Enrolled { version_id: String },
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
        Some(true) => text("version_id").map(|version_id| Outcome::Enrolled { version_id }),
        Some(false) => text("code").map(|code| Outcome::Refused { code }),
        None => None,
    }
    .ok_or_else(|| "The Sandbox manager's reply was malformed.".into())
}

/// The outcome in the `done` reply to `request_id` among `replies`,
/// skipping `accepted` and `progress` and whatever `open` refuses.
pub async fn done<T>(
    mut replies: impl Stream<Item = T> + Unpin,
    request_id: &str,
    open: impl Fn(&T) -> Option<Reply>,
) -> Result<Outcome, String> {
    while let Some(item) = replies.next().await {
        let Some(reply) = open(&item) else {
            continue;
        };
        if reply.request_id == request_id && reply.status == Status::Done {
            return outcome(reply.response.as_ref().unwrap_or(&Value::Null));
        }
    }
    Err("The community connection closed before the Sandbox manager answered.".into())
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
    let manager = PublicKey::from_hex(&manager).map_err(|_| "The manager key is not valid.")?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let request = identity
        .enroll_request(&manager, &request_id, profile, now)
        .map_err(|error| format!("Could not sign the enrollment ({}).", error.label()))?;
    if json!(["EVENT", request]).to_string().len() > MAX_PUBLICATION_FRAME {
        return Err("The profile is too large to send. Shorten the via profile.".into());
    }
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
        done(Box::pin(events), &request_id, |event| {
            identity
                .open_session_reply(event, &manager, &request.id)
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
