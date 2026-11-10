//! The developer's Paseo pairing, from the Sandbox manager's `enroll` or
//! `profile` reply. A manager that provisions Paseo daemons adds
//! `paseo: {password, server_id, offer_url}` to both replies (branch
//! `tee8z/paseo-provisioning`); an older manager adds nothing.

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
    match (text("password"), text("server_id"), text("offer_url")) {
        (Some(password), Some(server_id), Some(offer_url))
            if offer_url.starts_with(OFFER_PREFIX) && offer_url.len() > OFFER_PREFIX.len() =>
        {
            Ok(Some(Pairing {
                password: Secret::new(password.to_owned()),
                server_id: server_id.to_owned(),
                offer_url: offer_url.to_owned(),
            }))
        }
        _ => Err("The Sandbox manager's Paseo pairing was malformed.".into()),
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
