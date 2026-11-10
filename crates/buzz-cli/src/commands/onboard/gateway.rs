//! The gateway grant boundary. The owner delegates the Paseo daemon's
//! gateway key with an owner-signed, expiring event, so the daemon can drive
//! the owner's own remote agents and nothing else.
//!
//! The grant event belongs to `buzz_sdk::gateway` (branch
//! `tee8z/agent-gateway-key`, with `buzz gateway grant`), which is not in
//! this build. [`Unreleased`] says so, and the wizard prints the exact
//! command to run instead of pretending it granted anything. When the SDK
//! lands, implement [`GatewayGrants`] over it and make it the default in
//! [`super::run`].

use nostr::{Event, Keys, PublicKey};

/// The default grant lifetime.
pub const DEFAULT_LIFETIME: &str = "90d";

/// The longest grant `--gateway-expires` accepts.
pub const MAX_LIFETIME_SECS: u64 = 90 * DAY;

/// Re-grant when the saved grant expires within this long.
pub const RENEW_WITHIN_SECS: u64 = 7 * DAY;

const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;

/// Builds owner-signed gateway grants.
pub trait GatewayGrants {
    /// The owner-signed grant of `gateway` that expires at `expires_at`
    /// (unix seconds), created at `created_at`, ready to publish. `Ok(None)`
    /// when this build cannot sign one; `Err` when the grant is refused.
    fn grant(
        &self,
        owner: &Keys,
        gateway: &PublicKey,
        expires_at: u64,
        created_at: u64,
    ) -> Result<Option<Event>, String>;
}

/// This build: `buzz gateway grant` is not available yet.
pub struct Unreleased;

impl GatewayGrants for Unreleased {
    fn grant(
        &self,
        _owner: &Keys,
        _gateway: &PublicKey,
        _expires_at: u64,
        _created_at: u64,
    ) -> Result<Option<Event>, String> {
        Ok(None)
    }
}

/// A grant lifetime: `<n>h`, `<n>d` or `<n>w`, at least one hour and at most
/// [`MAX_LIFETIME_SECS`]. Returns the canonical text and the seconds.
pub fn lifetime(text: &str) -> Result<(String, u64), String> {
    let text = text.trim().to_ascii_lowercase();
    let refuse = || {
        format!(
            "--gateway-expires must be hours, days or weeks such as {DEFAULT_LIFETIME}, 36h or 2w, at most 90d"
        )
    };
    let (count, unit) = [("h", HOUR), ("d", DAY), ("w", 7 * DAY)]
        .into_iter()
        .find_map(|(suffix, unit)| text.strip_suffix(suffix).map(|count| (count, unit)))
        .ok_or_else(refuse)?;
    let count: u64 = count.parse().map_err(|_| refuse())?;
    let secs = count.checked_mul(unit).ok_or_else(refuse)?;
    if count == 0 || secs > MAX_LIFETIME_SECS {
        return Err(refuse());
    }
    Ok((text, secs))
}

/// The command that grants `gateway` for `lifetime` by hand.
pub fn command(gateway: &PublicKey, lifetime: &str) -> String {
    format!(
        "buzz gateway grant {} --expires {lifetime}",
        gateway.to_hex()
    )
}

/// Whether a grant that expires at `expires_at` must be renewed at `now`
/// for a run that grants `lifetime_secs`.
pub fn needs_renewal(expires_at: u64, now: u64, lifetime_secs: u64) -> bool {
    expires_at <= now.saturating_add(RENEW_WITHIN_SECS.min(lifetime_secs / 2))
}
