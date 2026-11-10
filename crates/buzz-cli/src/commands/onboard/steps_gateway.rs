//! Step 7: grant the Paseo daemon's gateway key.

use nostr::Keys;

use super::gateway;
use super::io::Prompter;
use super::manager::ControlTransport;
use super::paseo::Pairing;
use super::relay::{Relay, Signer};
use super::state::GatewayGrant;
use super::tools::Tools;
use super::Wizard;
use crate::error::CliError;

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Step 7. When the manager gave the Paseo daemon a gateway key, the
    /// owner grants it for `--gateway-expires` (default 90 days), so the
    /// daemon can drive the owner's remote agents. A saved grant for the same
    /// key is renewed only near its expiry. When this build cannot sign the
    /// grant, it prints the `buzz gateway grant` command and lists it under
    /// "Action needed" without asking; nothing is recorded as granted. A
    /// refused or failed publish fails the run and is retried on the next.
    pub async fn gateway_grant(
        &mut self,
        keys: &Keys,
        pairing: Option<&Pairing>,
    ) -> Result<(), CliError> {
        let Some(pairing) = pairing else {
            self.say("  No Paseo daemon yet, so there is no gateway key to grant.");
            return Ok(());
        };
        let Some(gateway) = pairing.gateway else {
            self.say("  Your Paseo daemon has no gateway key yet; nothing to grant.");
            return Ok(());
        };
        let hex = gateway.to_hex();
        if hex == self.state.owner_pubkey || self.state.agents.iter().any(|a| a.pubkey == hex) {
            self.actions.push(format!(
                "Your Paseo daemon's gateway key {hex} is your own or an agent's key; it was not granted. Ask an admin to check the Sandbox manager."
            ));
            return Ok(());
        }
        let (lifetime, lifetime_secs) = self.gateway_lifetime.clone();
        let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default();
        if let Some(saved) = &self.state.gateway_grant {
            if saved.gateway == hex && !gateway::needs_renewal(saved.expires_at, now, lifetime_secs)
            {
                self.say(&format!(
                    "  Your Paseo daemon's gateway key is granted until {}.",
                    date(saved.expires_at)
                ));
                return Ok(());
            }
        }
        let command = gateway::command(&gateway, &lifetime);
        if self.args.skip_gateway_grant {
            self.say(&format!("  Skipped. To grant it later, run: {command}"));
            return Ok(());
        }
        let expires_at = now.saturating_add(lifetime_secs);
        let event = self
            .grants
            .grant(keys, &gateway, expires_at, now)
            .map_err(|reason| {
                CliError::Other(format!(
                    "Could not grant the gateway key: {reason}. To grant it by hand, run: {command}"
                ))
            })?;
        let Some(event) = event else {
            self.say(
                "  Granting a gateway key (buzz gateway grant) is not available in this build yet.",
            );
            self.actions.push(format!(
                "Before granting, ask a relay administrator to admit gateway key {hex}: buzz-admin add-member --pubkey {hex} --role member. Add it to your workspace channel with buzz channels add-member --channel {} --pubkey {hex}. The grant does not add membership.",
                self.state.channel.as_ref().map_or("<channel id>", |channel| channel.id.as_str())
            ));
            self.actions.push(format!(
                "Grant your Paseo daemon's gateway key once buzz gateway grant is available (not in this build yet): {command}"
            ));
            return Ok(());
        };
        let question = format!(
            "  Let your Paseo daemon (gateway key {}…) drive your remote agents for {lifetime}?",
            hex.get(..12).unwrap_or(&hex)
        );
        if !self.prompter.confirm(&question, true)? {
            self.say(&format!("  Skipped. To grant it later, run: {command}"));
            return Ok(());
        }
        self.relay.publish(&Signer::owner(keys), event).await?;
        self.state.gateway_grant = Some(GatewayGrant {
            gateway: hex,
            expires_at,
        });
        self.save()?;
        self.say(&format!(
            "  Granted your Paseo daemon's gateway key until {}.",
            date(expires_at)
        ));
        Ok(())
    }
}

/// `secs` as a UTC date.
fn date(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(|| secs.to_string(), |at| at.format("%Y-%m-%d").to_string())
}
