//! The developer profile `enroll` stores: a port of `validate()` in
//! `voltage/images/buzz-agent/profile.py`. The Sandbox image validates the
//! profile again when it loads it, so a profile this accepts and the image
//! refuses would break every later session; the two must agree rule for
//! rule. Refusals are the image's messages, which never include a value.

use serde_json::{Map, Value};
use url::Url;

/// Largest profile, measured as Python's `json.dumps` writes it: the
/// Secrets Manager `SecretString` limit.
pub const MAX_BYTES: usize = 65_536;
const FIELDS: &[&str] = &[
    "version",
    "developer",
    "owner_pubkey",
    "git",
    "github",
    "via",
    "models",
];
const MODEL_FIELDS: &[&str] = &[
    "openai_api_key",
    "codex_access_token",
    "anthropic_api_key",
    "claude_oauth_token",
    "openrouter_api_key",
];

/// Why a profile was refused: one of `profile.py`'s messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid(pub &'static str);

fn require(condition: bool, message: &'static str) -> Result<(), Invalid> {
    if condition {
        Ok(())
    } else {
        Err(Invalid(message))
    }
}

/// An object with exactly `expected` as its keys.
fn fields<'a>(value: &'a Value, expected: &[&str]) -> Result<&'a Map<String, Value>, Invalid> {
    value
        .as_object()
        .filter(|map| map.len() == expected.len() && expected.iter().all(|f| map.contains_key(*f)))
        .ok_or(Invalid("Profile fields are incomplete or unsupported"))
}

/// Python's `str.isspace`: Unicode `White_Space` plus the four ASCII
/// separators U+001C to U+001F.
fn is_python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn token(value: &Value) -> Result<(), Invalid> {
    let ok = value.as_str().is_some_and(|value| {
        (1..=8192).contains(&value.chars().count()) && !value.chars().any(is_python_space)
    });
    require(ok, "Invalid credential")
}

fn is_developer_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    (1..=32).contains(&bytes.len())
        && bytes.iter().all(|b| alnum(b) || *b == b'-')
        && bytes.first().is_some_and(alnum)
        && bytes.last().is_some_and(alnum)
}

fn is_github_login(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=39).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// `profile.py`'s `SSH_SIGNING_KEY`: one OpenSSH private key block, its body
/// 1 to 4,000 base64 characters and newlines, with an optional final newline.
fn is_ssh_signing_key(value: &str) -> bool {
    const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n";
    const END: &str = "-----END OPENSSH PRIVATE KEY-----";
    let Some(rest) = value.strip_prefix(BEGIN) else {
        return false;
    };
    let rest = rest.strip_suffix('\n').unwrap_or(rest);
    let Some(body) = rest.strip_suffix(END) else {
        return false;
    };
    (1..=4000).contains(&body.len())
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n'))
}

/// Python truthiness of a TOML value.
fn truthy(value: Option<&toml::Value>) -> bool {
    match value {
        None => false,
        Some(toml::Value::String(s)) => !s.is_empty(),
        Some(toml::Value::Integer(n)) => *n != 0,
        Some(toml::Value::Float(n)) => *n != 0.0,
        Some(toml::Value::Boolean(b)) => *b,
        Some(toml::Value::Datetime(_)) => true,
        Some(toml::Value::Array(a)) => !a.is_empty(),
        Some(toml::Value::Table(t)) => !t.is_empty(),
    }
}

/// `profile.py`'s `validate(profile)`, in its order.
pub fn validate(profile: &Value) -> Result<(), Invalid> {
    let top = profile
        .as_object()
        .filter(|map| {
            FIELDS.iter().all(|field| map.contains_key(*field))
                && map
                    .keys()
                    .all(|field| FIELDS.contains(&field.as_str()) || field == "codex_login")
        })
        .ok_or(Invalid("Profile fields are incomplete or unsupported"))?;
    let version = &top["version"];
    require(
        (version.is_u64() || version.is_i64()) && version.as_i64() == Some(1),
        "Unsupported profile version",
    )?;
    require(
        top["developer"].as_str().is_some_and(is_developer_id),
        "Invalid developer ID",
    )?;
    require(
        top["owner_pubkey"].as_str().is_some_and(|key| {
            key.len() == 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }),
        "Invalid Buzz owner",
    )?;
    let git = fields(&top["git"], &["name", "email", "ssh_signing_key"])?;
    for value in [&git["name"], &git["email"]] {
        require(
            value.as_str().is_some_and(|value| {
                (1..=256).contains(&value.chars().count()) && !value.chars().any(|c| c < ' ')
            }),
            "Invalid Git identity",
        )?;
    }
    require(
        git["ssh_signing_key"]
            .as_str()
            .is_some_and(is_ssh_signing_key),
        "Invalid SSH signing key",
    )?;
    let github = fields(&top["github"], &["login", "token"])?;
    require(
        github["login"].as_str().is_some_and(is_github_login),
        "Invalid GitHub login",
    )?;
    token(&github["token"])?;
    let via = fields(&top["via"], &["profile", "op_token"])?;
    token(&via["op_token"])?;
    let via_profile = via["profile"]
        .as_str()
        .ok_or(Invalid("Invalid via profile"))?;
    let config: toml::Table =
        toml::from_str(via_profile).map_err(|_| Invalid("Invalid via profile"))?;
    require(
        truthy(config.get("services")) && truthy(config.get("providers")),
        "via profile requires providers and services",
    )?;
    let models = top["models"]
        .as_object()
        .filter(|models| models.keys().all(|k| MODEL_FIELDS.contains(&k.as_str())))
        .ok_or(Invalid("Unsupported model credential type"))?;
    if let Some(login) = top.get("codex_login") {
        validate_codex_login(login, top["developer"].as_str().unwrap_or_default())?;
        require(
            !models.contains_key("openai_api_key") && !models.contains_key("codex_access_token"),
            "Choose a ChatGPT login, an OpenAI API key, or a Codex access token",
        )?;
    }
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
        python_json_len(profile) <= MAX_BYTES,
        "Profile exceeds Secrets Manager size limit",
    )
}

/// Broker metadata only. The upstream refresh token belongs in the
/// manager-only login secret, never this agent-readable profile.
fn validate_codex_login(value: &Value, developer: &str) -> Result<(), Invalid> {
    let login = fields(value, &["url", "ca_certificate", "refresh_token"])?;
    let url = login["url"]
        .as_str()
        .and_then(|value| Url::parse(value).ok())
        .ok_or(Invalid("Invalid ChatGPT broker URL"))?;
    require(
        url.scheme() == "https"
            && url.host_str() == Some("codex-refresh.buzz-sandbox-manager.svc.cluster.local")
            && url.port() == Some(8443)
            && url.path() == "/oauth/token"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Invalid ChatGPT broker URL",
    )?;
    require(
        login["ca_certificate"].as_str().is_some_and(|certificate| {
            certificate.len() <= 8192
                && certificate.starts_with("-----BEGIN CERTIFICATE-----\n")
                && certificate
                    .trim_end()
                    .ends_with("-----END CERTIFICATE-----")
        }),
        "Invalid ChatGPT broker certificate",
    )?;
    token(&login["refresh_token"])?;
    let credential = login["refresh_token"].as_str().unwrap_or_default();
    let prefix = format!("{developer}.");
    require(
        credential.strip_prefix(&prefix).is_some_and(|body| {
            body.strip_suffix(".seed").is_some_and(|random| {
                random.len() == 32
                    && random
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
        }),
        "Invalid ChatGPT broker credential",
    )
}

/// The byte length of Python's `json.dumps(value)`: `", "` and `": "`
/// separators, and every character outside `' '..='~'` escaped, as `\uXXXX`
/// (a surrogate pair above U+FFFF) unless it has a short escape.
fn python_json_len(value: &Value) -> usize {
    let separators = |n: usize| 2 * n.saturating_sub(1);
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => python_string_len(s),
        Value::Array(items) => {
            2 + items.iter().map(python_json_len).sum::<usize>() + separators(items.len())
        }
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| python_string_len(k) + 2 + python_json_len(v))
                .sum::<usize>()
                + separators(map.len())
        }
    }
}

fn python_string_len(value: &str) -> usize {
    2 + value
        .chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            ' '..='~' => 1,
            c if (c as u32) > 0xFFFF => 12,
            _ => 6,
        })
        .sum::<usize>()
}

/// The sections of a profile and the keys each holds.
const SECTIONS: &[(&str, &[&str])] = &[
    ("git", &["name", "email", "ssh_signing_key"]),
    ("github", &["login", "token"]),
    ("via", &["profile", "op_token"]),
];

/// Model credentials a profile may hold only one of, each with its other.
const EXCLUSIVE_MODELS: &[(&str, &str)] = &[
    ("openai_api_key", "codex_access_token"),
    ("anthropic_api_key", "claude_oauth_token"),
];

/// The shape of a partial profile `enroll` merges into the stored one:
/// `profile.py`'s fields, each optional, with `null` removing a model
/// credential. [`validate`] checks the merged profile's values.
pub fn check_partial(profile: &Value) -> Result<(), Invalid> {
    const INCOMPLETE: &str = "Profile fields are incomplete or unsupported";
    let top = profile.as_object().ok_or(Invalid(INCOMPLETE))?;
    require(
        top.keys()
            .all(|k| FIELDS.contains(&k.as_str()) || k == "codex_login"),
        INCOMPLETE,
    )?;
    // Clients may disconnect, but only the manager creates broker metadata.
    require(
        top.get("codex_login").is_none_or(Value::is_null),
        "ChatGPT broker metadata is managed by the Sandbox manager",
    )?;
    let version = top.get("version").unwrap_or(&Value::Null);
    require(
        (version.is_u64() || version.is_i64()) && version.as_i64() == Some(1),
        "Unsupported profile version",
    )?;
    for (section, keys) in SECTIONS {
        if let Some(value) = top.get(*section) {
            let map = value.as_object().ok_or(Invalid(INCOMPLETE))?;
            require(map.keys().all(|k| keys.contains(&k.as_str())), INCOMPLETE)?;
        }
    }
    // A new token is checked against its login, so the two travel together.
    if let Some(github) = top.get("github") {
        require(
            github.get("login").is_some() && github.get("token").is_some(),
            "A GitHub token needs its login",
        )?;
    }
    if let Some(models) = top.get("models") {
        let ok = models.as_object().is_some_and(|models| {
            models.iter().all(|(key, value)| {
                MODEL_FIELDS.contains(&key.as_str()) && (value.is_string() || value.is_null())
            })
        });
        require(ok, "Unsupported model credential type")?;
    }
    Ok(())
}

/// `partial` over `stored`: a field `partial` names replaces the stored one,
/// and any other is kept. In `models`, `null` removes a credential, and
/// setting one of an exclusive pair removes the other.
pub fn merge(stored: Option<Value>, partial: Value) -> Value {
    let mut merged = stored
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Value::Object(partial) = partial else {
        return merged;
    };
    for (field, value) in partial {
        let section = SECTIONS.iter().any(|(name, _)| *name == field);
        if !section && field != "models" {
            merged[&field] = value;
            continue;
        }
        if !merged[&field].is_object() {
            merged[&field] = Value::Object(Map::new());
        }
        let (Some(target), Value::Object(values)) = (merged[&field].as_object_mut(), value) else {
            continue;
        };
        for (key, value) in values {
            if field != "models" {
                target.insert(key, value);
            } else if value.is_null() {
                target.remove(&key);
            } else {
                for (one, other) in EXCLUSIVE_MODELS {
                    if key == *one {
                        target.remove(*other);
                    } else if key == *other {
                        target.remove(*one);
                    }
                }
                target.insert(key, value);
            }
        }
    }
    // A profile may hold no model credentials.
    if !merged["models"].is_object() {
        merged["models"] = Value::Object(Map::new());
    }
    merged
}

/// What `profile` holds, for its owner: the GitHub login and Git identity,
/// and which credentials are present. It names no credential value.
pub fn summary(profile: &Value, version_id: &str, updated_at: Option<String>) -> Value {
    let present = |pointer: &str| {
        profile
            .pointer(pointer)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    let text = |pointer: &str| profile.pointer(pointer).cloned().unwrap_or(Value::Null);
    let mut models: Vec<&String> = profile["models"]
        .as_object()
        .map(|models| models.keys().collect())
        .unwrap_or_default();
    models.sort();
    let mut summary = serde_json::json!({
        "version_id": version_id,
        "updated_at": updated_at,
        "git": {
            "name": text("/git/name"),
            "email": text("/git/email"),
            "signing_key": present("/git/ssh_signing_key"),
        },
        "github": {"login": text("/github/login"), "token": present("/github/token")},
        "via": {"profile": present("/via/profile"), "op_token": present("/via/op_token")},
        "models": models,
    });
    if profile.get("codex_login").is_some_and(Value::is_object) {
        summary["codex_login"] = Value::Bool(true);
    }
    summary
}

#[cfg(test)]
#[path = "profile_tests.rs"]
pub(crate) mod tests;
