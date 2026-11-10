//! The daemon Secret's allowlisted environment and public pairing reply.

use super::*;

const PAIRING_FIELDS: &[(&str, &str)] = &[
    ("password", "PASEO_PASSWORD"),
    ("server_id", "PASEO_SERVER_ID"),
    ("daemon_keypair", "PASEO_DAEMON_KEYPAIR"),
];
const GATEWAY_KEY: &str = "BUZZ_PRIVATE_KEY";

pub(super) fn read_env(secret: &Secret) -> Result<BTreeMap<String, String>, Failed> {
    secret.data.as_ref().map_or_else(
        || Ok(BTreeMap::new()),
        |data| {
            data.iter()
                .map(|(key, value)| {
                    String::from_utf8(value.0.clone())
                        .map(|value| (key.clone(), value))
                        .map_err(|_| Failed("invalid_secret"))
                })
                .collect()
        },
    )
}

/// Generate identity only for a new Secret. Malformed
/// existing fields fail closed; a read error never reaches this function.
pub(super) fn daemon_env(
    profile: &Value,
    agents: &Value,
    prior: Option<BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>, Failed> {
    let existing = prior.is_some();
    let mut env = prior.unwrap_or_default();
    // Codex is disabled for the daemon; preserve identity while retiring its old token.
    env.remove("CODEX_ACCESS_TOKEN");
    if env.keys().any(|key| {
        !PAIRING_FIELDS.iter().any(|(_, name)| name == key)
            && !MODEL_TOKENS.iter().any(|(_, name)| name == key)
            && key != AGENTS_KEY
            && key != GATEWAY_KEY
            && key != GIT_READ_TOKEN_KEY
    }) {
        return Err(Failed("unsupported_secret_key"));
    }
    if existing {
        // A partial Secret cannot identify which generated identity the owner
        // paired or delegated. Repair must never silently replace any part.
        if PAIRING_FIELDS
            .iter()
            .any(|(_, key)| !env.contains_key(*key))
            || !env.contains_key(GATEWAY_KEY)
        {
            return Err(Failed("invalid_pairing"));
        }
    } else {
        let generated = generate()?;
        for (field, key) in PAIRING_FIELDS {
            let value = if *field == "daemon_keypair" {
                generated[field].to_string()
            } else {
                generated[field].as_str().unwrap_or_default().to_owned()
            };
            env.insert((*key).to_owned(), value);
        }
        let keys = nostr::Keys::generate();
        env.insert(GATEWAY_KEY.to_owned(), keys.secret_key().to_secret_hex());
    }
    if env
        .get(GIT_READ_TOKEN_KEY)
        .is_some_and(|token| !valid_git_read_token(token))
    {
        return Err(Failed("invalid_git_read_token"));
    }
    // Validate the saved values, including public/secret key consistency.
    pairing(&env, RELAY_ENDPOINT)?;
    env.insert(AGENTS_KEY.to_owned(), agents.to_string());
    for (field, variable) in MODEL_TOKENS {
        env.remove(*variable);
        if let Some(token) = profile["models"][field].as_str() {
            env.insert((*variable).to_owned(), token.to_owned());
        }
    }
    Ok(env)
}

pub(super) fn pairing(env: &BTreeMap<String, String>, endpoint: &str) -> Result<Value, Failed> {
    let text = |key: &str| {
        env.get(key)
            .map(String::as_str)
            .ok_or(Failed("invalid_pairing"))
    };
    let password = text("PASEO_PASSWORD")?;
    let server_id = text("PASEO_SERVER_ID")?;
    if !URL_SAFE_NO_PAD
        .decode(password)
        .is_ok_and(|raw| raw.len() == 32)
        || server_id
            .strip_prefix("srv_")
            .is_none_or(|id| !URL_SAFE_NO_PAD.decode(id).is_ok_and(|raw| raw.len() == 9))
    {
        return Err(Failed("invalid_pairing"));
    }
    let pair: Value = serde_json::from_str(text("PASEO_DAEMON_KEYPAIR")?)
        .map_err(|_| Failed("invalid_pairing"))?;
    let secret: [u8; 32] = pair["secretKeyB64"]
        .as_str()
        .and_then(|secret| STANDARD.decode(secret).ok())
        .and_then(|raw| raw.try_into().ok())
        .ok_or(Failed("invalid_pairing"))?;
    if pair != keypair(&secret) {
        return Err(Failed("invalid_pairing"));
    }
    let gateway = text(GATEWAY_KEY)?;
    if gateway.len() != 64 || !gateway.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Failed("invalid_gateway_key"));
    }
    let keys = nostr::Keys::parse(gateway).map_err(|_| Failed("invalid_gateway_key"))?;
    Ok(json!({
        "password": password,
        "server_id": server_id,
        "offer_url": offer_url(server_id, pair["publicKeyB64"].as_str().unwrap_or_default(), endpoint),
        "gateway_pubkey": keys.public_key().to_hex(),
    }))
}
