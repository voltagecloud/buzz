//! The developer profile `enroll` stores: a port of `validate()` in
//! `voltage/images/buzz-agent/profile.py`. The Sandbox image validates the
//! profile again when it loads it, so a profile this accepts and the image
//! refuses would break every later session; the two must agree rule for
//! rule. Refusals are the image's messages, which never include a value.

use serde_json::{Map, Value};

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
    "depot",
    "models",
];
const MODEL_FIELDS: &[&str] = &["openai_api_key", "anthropic_api_key", "claude_oauth_token"];

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
    let top = fields(profile, FIELDS)?;
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
    let git = fields(&top["git"], &["name", "email", "signing_key"])?;
    for value in git.values() {
        require(
            value.as_str().is_some_and(|value| {
                (1..=256).contains(&value.chars().count()) && !value.chars().any(|c| c < ' ')
            }),
            "Invalid Git identity",
        )?;
    }
    require(
        git["signing_key"].as_str().is_some_and(|key| {
            (40..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_hexdigit())
        }),
        "Use a full OpenPGP signing fingerprint",
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
    let depot = fields(&top["depot"], &["organization", "token"])?;
    require(
        depot["organization"].as_str().is_some_and(|org| {
            (1..=64).contains(&org.len())
                && org
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }),
        "Invalid Depot organization",
    )?;
    token(&depot["token"])?;
    let models = top["models"]
        .as_object()
        .filter(|models| models.keys().all(|k| MODEL_FIELDS.contains(&k.as_str())))
        .ok_or(Invalid("Unsupported model credential type"))?;
    // Claude Code prefers an API key over the subscription token, so allow only one.
    require(
        !(models.contains_key("anthropic_api_key") && models.contains_key("claude_oauth_token")),
        "Choose either a Claude API key or a Claude subscription token",
    )?;
    for value in models.values() {
        token(value)?;
    }
    require(
        python_json_len(profile) <= MAX_BYTES,
        "Profile exceeds Secrets Manager size limit",
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

#[cfg(test)]
#[path = "profile_tests.rs"]
pub(crate) mod tests;
