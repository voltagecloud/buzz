//! Step 7: grant the Paseo daemon's gateway key.

use buzz_sdk::gateway::{Delegation, MAX_FUTURE_SKEW_SECS};
use nostr::{Event, Keys};
use serde_json::json;

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
    /// Reconcile the current relay grant before reporting authority. Local
    /// onboarding state is a summary, never proof of an unrevoked delegation.
    pub async fn gateway_grant(
        &mut self,
        keys: &Keys,
        pairing: Option<&Pairing>,
    ) -> Result<(), CliError> {
        if self.state.gateway_grant.take().is_some() {
            self.save()?;
        }
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
        let command = gateway::command(&gateway, &lifetime);
        if self.args.skip_gateway_grant {
            self.say(&format!("  Skipped. To grant it later, run: {command}"));
            return Ok(());
        }
        self.actions.push(format!(
            "Gateway membership is separate: ask a relay administrator to admit {hex} with buzz-admin add-member --pubkey {hex} --role member, then add it to the workspace with buzz channels add-member --channel {} --pubkey {hex}.",
            self.state.channel.as_ref().map_or("<channel id>", |channel| channel.id.as_str())
        ));
        let profiles = self
            .relay
            .query(keys, json!({"kinds":[0], "authors":[hex], "limit":1}))
            .await?;
        if profiles.iter().any(|profile| {
            profile["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag[0] == "auth"))
        }) {
            return Err(CliError::Auth("the daemon gateway key belongs to an agent; ask an administrator to repair its Secret".into()));
        }
        let values = self
            .relay
            .query(
                keys,
                json!({
                    "kinds":[buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION],
                    "authors":[keys.public_key()], "#d":[hex], "limit":1
                }),
            )
            .await?;
        let heads: Vec<Event> = values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|_| CliError::Other("relay returned an invalid gateway grant".into()))?;
        if heads.len() > 1 {
            return Err(CliError::Other(
                "relay returned multiple gateway heads".into(),
            ));
        }
        let mut created_at = now;
        if let Some(head) = heads.first() {
            let (delegation, _) = Delegation::parse(head).map_err(|error| {
                CliError::Other(format!("invalid current gateway head: {error}"))
            })?;
            if head.pubkey != keys.public_key() || delegation.gateway != gateway {
                return Err(CliError::Other(
                    "relay returned an unrelated gateway head".into(),
                ));
            }
            if let Some(live) = buzz_sdk::gateway::live(head, keys.public_key(), gateway, now) {
                if !gateway::needs_renewal(live.expires_at(), now, lifetime_secs) {
                    self.state.gateway_grant = Some(GatewayGrant {
                        gateway: hex,
                        expires_at: live.expires_at(),
                    });
                    self.save()?;
                    self.say(&format!(
                        "  Your Paseo daemon's gateway key is granted until {}.",
                        date(live.expires_at())
                    ));
                    return Ok(());
                }
            }
            created_at = now.max(head.created_at.as_secs().saturating_add(1));
            if created_at > now.saturating_add(MAX_FUTURE_SKEW_SECS) {
                return Err(CliError::Conflict(
                    "the current gateway head is too far in the future; check the owner's clock"
                        .into(),
                ));
            }
        }
        let expires_at = now.saturating_add(lifetime_secs);
        let builder = Delegation::grant(gateway, expires_at, created_at)
            .map_err(|error| CliError::Other(format!("could not build gateway grant: {error}")))?;
        let event = builder
            .sign_with_keys(keys)
            .map_err(|error| CliError::Other(format!("could not sign gateway grant: {error}")))?;
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
