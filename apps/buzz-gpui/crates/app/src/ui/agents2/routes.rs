//! The Agents page's routes (buzz-app's `routes.ts`): `{"provider":
//! "<plugin>/<type>"}` opens that type's provider dialog. The port adds
//! `{"agent": "<pubkey>"}`, which opens that agent's Build view, so
//! another page can link to an agent.

use serde_json::{Value, json};

fn only<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    let fields = params.as_object()?;
    if fields.len() != 1 {
        return None;
    }
    fields.get(key)?.as_str()
}

/// `<plugin>/<type>`, each part as `^[a-z0-9][a-z0-9._-]*$`.
fn provider_key(key: &str) -> bool {
    let part = |part: &str| {
        let mut bytes = part.bytes();
        bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    };
    matches!(key.split_once('/'), Some((plugin, kind)) if part(plugin) && part(kind))
}

/// The type key a provider route names (`agentProviderRoute`).
pub fn provider_route(params: &Value) -> Option<&str> {
    only(params, "provider").filter(|key| provider_key(key))
}

/// The agent an agent route names: a 64-character lowercase hex pubkey.
pub fn agent_route(params: &Value) -> Option<&str> {
    only(params, "agent").filter(|pubkey| {
        pubkey.len() == 64
            && pubkey
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Whether the Agents page can open `params`. A provider route is valid
/// even for a type that is not registered; its dialog then shows nothing.
pub fn valid(params: &Value) -> bool {
    provider_route(params).is_some() || agent_route(params).is_some()
}

pub fn provider_params(key: &str) -> Value {
    json!({ "provider": key })
}

pub fn agent_params(pubkey: &str) -> Value {
    json!({ "agent": pubkey })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn admits_provider_shaped_routes() {
        let route = json!({ "provider": "test/echo" });
        assert_eq!(provider_route(&route), Some("test/echo"));
        assert!(valid(&route));
        assert!(valid(&json!({ "provider": "test/gone" })));
        for invalid in [
            Value::Null,
            json!("test/echo"),
            json!({ "provider": "echo" }),
            json!({ "provider": "test/echo", "extra": true }),
            json!({ "provider": "Test/echo" }),
            json!({ "provider": "test/.echo" }),
            json!({ "provider": "a/b/c" }),
            json!(["provider", "test/echo"]),
        ] {
            assert_eq!(provider_route(&invalid), None, "{invalid}");
        }
        assert_eq!(provider_params("test/echo"), route);
    }

    #[test]
    fn admits_an_agent_by_its_hex_pubkey() {
        let pubkey = "ab".repeat(32);
        let route = agent_params(&pubkey);
        assert_eq!(agent_route(&route), Some(pubkey.as_str()));
        assert!(valid(&route));
        for invalid in [
            json!({ "agent": "ab".repeat(31) }),
            json!({ "agent": "AB".repeat(32) }),
            json!({ "agent": "zz".repeat(32) }),
            json!({ "agent": pubkey, "provider": "test/echo" }),
            json!({ "agent": 1 }),
        ] {
            assert!(!valid(&invalid), "{invalid}");
        }
    }
}
