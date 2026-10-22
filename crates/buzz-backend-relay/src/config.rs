//! `provider_config` for the relay provider and its `info` schema.
//!
//! The provider only needs to reach the session manager: the relay, the
//! manager's key, and how long to wait. The manager decides the namespace,
//! the image and the Pod settings, so none of them is accepted here.

use std::time::Duration;

use nostr::PublicKey;
use serde_json::Value;

/// Default and largest `timeout_seconds`.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_TIMEOUT_SECONDS: u64 = 900;

/// Every accepted field. `sandbox` and `identity_policy` keep the Kubernetes
/// provider's shape and rules and are forwarded to the manager.
const FIELDS: &[&str] = &[
    "relay_url",
    "manager_pubkey",
    "timeout_seconds",
    "sandbox",
    "identity_policy",
];

/// The fields forwarded to the manager in the request payload.
pub const FORWARDED: &[&str] = &["sandbox", "identity_policy"];

/// A validated relay provider configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub relay_url: String,
    pub manager: PublicKey,
    /// How long the manager has to finish a request once it is sent.
    pub timeout: Duration,
}

/// Parse and validate. Every refusal names the field, never its value.
pub fn parse(config: &Value) -> Result<RelayConfig, String> {
    let fields = config
        .as_object()
        .ok_or("provider_config must be a JSON object")?;
    if let Some(field) = fields.keys().find(|f| !FIELDS.contains(&f.as_str())) {
        return Err(format!(
            "provider_config.{field} is not a relay provider setting; the session manager decides the namespace, image and Pod settings"
        ));
    }
    let relay_url = fields
        .get("relay_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| relay_url_allowed(url))
        .ok_or("provider_config.relay_url must be a wss:// URL (ws:// only for a loopback relay)")?
        .to_string();
    let manager = fields
        .get("manager_pubkey")
        .and_then(Value::as_str)
        .filter(|key| is_lowercase_hex_key(key))
        .and_then(|key| PublicKey::from_hex(key).ok())
        .ok_or("provider_config.manager_pubkey must be 64 lowercase hex characters")?;
    let timeout = match fields.get("timeout_seconds") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECONDS,
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
            .filter(|n| (1..=MAX_TIMEOUT_SECONDS).contains(n))
            .ok_or("provider_config.timeout_seconds must be an integer from 1 to 900")?,
    };
    Ok(RelayConfig {
        relay_url,
        manager,
        timeout: Duration::from_secs(timeout),
    })
}

fn is_lowercase_hex_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `wss://` anywhere; `ws://` only to this machine, for a local relay.
fn relay_url_allowed(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    match url.scheme() {
        "wss" => url.host().is_some(),
        "ws" => match url.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// The `config_schema` returned by `info`. `sandbox` and `identity_policy`
/// have the Kubernetes provider's shapes.
pub fn config_schema() -> Value {
    let key = serde_json::json!({"type": "string", "pattern": "^[0-9a-f]{64}$"});
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "relay_url": {"type": "string", "title": "Relay URL",
                "description": "The community relay the session manager listens on (wss://)."},
            "manager_pubkey": {"type": "string", "pattern": "^[0-9a-f]{64}$",
                "title": "Session manager public key",
                "description": "Hex public key of the session manager. Requests are encrypted to it."},
            "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECONDS,
                "default": DEFAULT_TIMEOUT_SECONDS, "title": "Request timeout (seconds)",
                "description": "How long the session manager has to finish a request."},
            "sandbox": {
                "oneOf": [{"const": false}, {
                    "type": "object", "additionalProperties": false,
                    "required": ["channel_id", "thread_root"],
                    "properties": {
                        "channel_id": {"type": "string", "format": "uuid",
                            "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"},
                        "thread_root": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                        "recovery": {"type": "object", "additionalProperties": false,
                            "required": ["mode"],
                            "properties": {"mode": {"type": "string", "enum": ["checkpoint", "fresh"]}}}
                    }
                }],
                "x-buzz-ui-type": "boolean", "default": true, "title": "Workspace per thread",
                "description": "The session manager runs Agent Sandbox sessions only. Desktop stores true and binds the mentioned thread at launch; deploy accepts only {\"channel_id\": \"<UUID>\", \"thread_root\": \"<event ID>\"}."
            },
            "identity_policy": {
                "type": "object", "additionalProperties": false,
                "required": ["agent_pubkey", "owner_pubkey"],
                "properties": {"agent_pubkey": key, "owner_pubkey": key},
                "description": "Approved agent and owner public keys, checked before the request is sent. Required."
            }
        },
        "required": ["relay_url", "manager_pubkey"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> Value {
        json!({"relay_url": "wss://relay.example.com", "manager_pubkey": "a".repeat(64)})
    }

    #[test]
    fn parses_the_minimal_config_with_the_default_timeout() {
        let config = parse(&valid()).unwrap();
        assert_eq!(config.relay_url, "wss://relay.example.com");
        assert_eq!(config.manager.to_hex(), "a".repeat(64));
        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECONDS));
        let mut value = valid();
        value["timeout_seconds"] = json!("900");
        assert_eq!(parse(&value).unwrap().timeout, Duration::from_secs(900));
    }

    #[test]
    fn refuses_each_invalid_field_and_names_it() {
        let cases = [
            ("relay_url", json!("https://relay.example.com")),
            ("relay_url", json!("ws://relay.example.com")),
            ("relay_url", json!("wss://")),
            ("relay_url", json!(7)),
            ("manager_pubkey", json!("A".repeat(64))),
            ("manager_pubkey", json!("a".repeat(63))),
            ("timeout_seconds", json!(0)),
            ("timeout_seconds", json!(901)),
            ("timeout_seconds", json!(-1)),
            ("timeout_seconds", json!(1.5)),
            ("namespace", json!("buzz-agent-staging-someone")),
            ("image", json!("example.invalid/x@sha256:00")),
            ("context", json!("admin")),
        ];
        for (field, bad) in cases {
            let mut value = valid();
            value[field] = bad;
            let error = parse(&value).unwrap_err();
            assert!(error.contains(field), "{field}: {error}");
        }
        for missing in ["relay_url", "manager_pubkey"] {
            let mut value = valid();
            value.as_object_mut().unwrap().remove(missing);
            assert!(parse(&value).unwrap_err().contains(missing));
        }
        assert!(parse(&json!([])).is_err());
    }

    #[test]
    fn plain_websocket_is_only_for_a_loopback_relay() {
        for (url, allowed) in [
            ("ws://127.0.0.1:3000", true),
            ("ws://localhost:3000", true),
            ("ws://[::1]:3000", true),
            ("ws://10.0.0.1:3000", false),
            ("ws://localhost.example.com", false),
            ("wss://10.0.0.1", true),
        ] {
            assert_eq!(relay_url_allowed(url), allowed, "{url}");
        }
    }

    #[test]
    fn the_schema_offers_exactly_the_accepted_fields() {
        let schema = config_schema();
        let mut properties: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort_unstable();
        let mut fields = FIELDS.to_vec();
        fields.sort_unstable();
        assert_eq!(properties, fields);
    }
}
