//! The developer's Paseo pairing, from the Sandbox manager's `enroll` or
//! `profile` reply. A manager that provisions Paseo daemons adds
//! `paseo: {password, server_id, offer_url, gateway_pubkey?}` to both
//! replies (voltagecloud/buzz#150); an older manager adds nothing.

use nostr::PublicKey;
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;
use serde_json::Value;

use super::state::Secret;

/// The Paseo app's only offer origin.
pub const OFFER_PREFIX: &str = "https://app.paseo.sh/#offer=";

/// How to pair a Paseo app with the developer's daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    /// The daemon password. Shown once, never saved.
    pub password: Secret,
    /// The daemon's server ID (not secret).
    pub server_id: String,
    /// `https://app.paseo.sh/#offer=…`, which carries only public values.
    pub offer_url: String,
    /// The daemon's gateway key, when the manager gave it one. The owner
    /// grants it ([`super::gateway`]) so the daemon can drive their agents.
    pub gateway: Option<PublicKey>,
}

/// The pairing in a manager reply. `Ok(None)` when the reply has none; an
/// `Err` when it has one that is malformed, so a broken pairing is reported
/// rather than mistaken for "not provisioned".
pub fn pairing(response: &Value) -> Result<Option<Pairing>, String> {
    let Some(paseo) = response.get("paseo").filter(|paseo| !paseo.is_null()) else {
        return Ok(None);
    };
    let text = |name: &str| {
        paseo
            .get(name)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let malformed = || "The Sandbox manager's Paseo pairing was malformed.".to_owned();
    // Absent or null means no gateway key yet; anything else must be a key.
    let gateway = match paseo.get("gateway_pubkey") {
        None | Some(Value::Null) => None,
        Some(Value::String(hex)) => Some(PublicKey::from_hex(hex).map_err(|_| malformed())?),
        Some(_) => return Err(malformed()),
    };
    match (text("password"), text("server_id"), text("offer_url")) {
        (Some(password), Some(server_id), Some(offer_url))
            if offer_url.starts_with(OFFER_PREFIX) && offer_url.len() > OFFER_PREFIX.len() =>
        {
            Ok(Some(Pairing {
                password: Secret::new(password.to_owned()),
                server_id: server_id.to_owned(),
                offer_url: offer_url.to_owned(),
                gateway,
            }))
        }
        _ => Err(malformed()),
    }
}

/// `url` as a terminal QR code, two modules per character row.
pub fn qr(url: &str) -> Result<String, String> {
    let code =
        QrCode::new(url.as_bytes()).map_err(|_| "The offer URL is too long for a QR code.")?;
    Ok(code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}
