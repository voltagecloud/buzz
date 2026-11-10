//! Step 6 (enroll, the signing key on GitHub, the Paseo pairing) and
//! step 8 (next steps).

use buzz_session_control::Op;
use nostr::Keys;
use serde_json::json;

use super::io::Prompter;
use super::manager::{self, ControlTransport};
use super::paseo::{self, Pairing};
use super::profile::{self, Changes, Outcome};
use super::relay::{Relay, Signer};
use super::state::PendingKey;
use super::steps_credentials::Credentials;
use super::steps_identity::OnFile;
use super::tools::Tools;
use super::Wizard;
use crate::error::CliError;

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Step 6. Sends the owner-signed, NIP-44 encrypted kind 24210 `enroll`
    /// the Buzz app sends: the complete profile when nothing is on file, or
    /// only what changes. Once the manager stores a new signing key, adds its
    /// public half to GitHub with `gh` (or hands back the manual steps and
    /// retries on the next run). Returns the Paseo pairing, when provisioned.
    pub async fn enroll(
        &mut self,
        keys: &Keys,
        on_file: &OnFile,
        credentials: Credentials,
    ) -> Result<Option<Pairing>, CliError> {
        let mut pairing = on_file.pairing.clone();
        if on_file.summary.is_some()
            && !credentials.changes()
            && !self.args.reenroll
            && !(on_file.paseo_enabled && pairing.is_none())
        {
            self.say("  Your profile is on file; nothing new to send.");
            self.register_signing_key().await;
            return Ok(pairing);
        }
        let owner = keys.public_key().to_hex();
        let body = match &on_file.summary {
            None => {
                let github = credentials
                    .github
                    .as_ref()
                    .ok_or_else(|| CliError::Usage("A GitHub token is required.".into()))?;
                let key = credentials
                    .signing_key
                    .as_ref()
                    .ok_or_else(|| CliError::Other("No signing key was created.".into()))?;
                profile::profile(&credentials.values, &owner, github, &key.private)
            }
            Some(summary) => {
                // Re-enrollment also provisions or rotates the narrow daemon
                // Secret. Preserve credentials by sending the existing email.
                let mut unchanged = profile::Values::default();
                let values = if credentials.changes() {
                    &credentials.values
                } else {
                    unchanged.set(profile::Field::GitEmail, summary.git.email.clone());
                    &unchanged
                };
                profile::partial(
                    values,
                    &owner,
                    Changes {
                        github: credentials.github.as_ref(),
                        signing_key: credentials
                            .signing_key
                            .as_ref()
                            .map(|key| key.private.as_str()),
                    },
                )
            }
        }
        .map_err(CliError::Usage)?;
        let response = manager::ask(
            &self.transport,
            &self.manager,
            &Signer::owner(keys),
            Op::Enroll,
            json!({ "profile": body }),
            manager::OWNER_TIMEOUT,
        )
        .await?;
        let outcome = profile::outcome(&response).map_err(CliError::Other)?;
        let version_id = match &outcome {
            Outcome::Refused { code } if code == "unauthorized_owner" => {
                return Err(CliError::Auth(format!(
                    "{} Your owner key is {owner}.",
                    outcome.text()
                )));
            }
            Outcome::Refused { .. } => return Err(CliError::Other(outcome.text())),
            Outcome::Enrolled { version_id } => version_id.clone(),
        };
        self.say(&format!("  {}", outcome.text()));
        self.state.enrolled_version = Some(version_id);
        match paseo::pairing(&response) {
            Ok(Some(found)) => pairing = Some(found),
            Ok(None) => {}
            Err(error) if self.actions.contains(&error) => {}
            Err(error) => self.actions.push(error),
        }
        if let Some(key) = &credentials.signing_key {
            self.state.pending_key = Some(PendingKey {
                login: credentials.login.clone(),
                title: format!(
                    "Buzz agents ({})",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
                ),
                public: key.public.clone(),
            });
        }
        if let Err(error) = self.save() {
            // The manager stored the key; without the saved record, say
            // where its public half must go before failing.
            if let Some(key) = &self.state.pending_key {
                self.say(&format!(
                    "  Add this signing key to GitHub @{} as a Signing Key: {}",
                    key.login, key.public
                ));
            }
            return Err(error);
        }
        drop(credentials);
        self.register_signing_key().await;
        Ok(pairing)
    }

    /// Adds the stored signing key's public half to GitHub with `gh`. A key
    /// `gh` cannot add stays pending, with the manual steps reported.
    async fn register_signing_key(&mut self) {
        let Some(key) = self.state.pending_key.clone() else {
            return;
        };
        let added = self
            .tools
            .gh_add_signing_key(&key.login, &key.title, &key.public)
            .await;
        match added {
            Ok(()) | Err("this key is already on a GitHub account") => {
                self.say(&format!(
                    "  Your agents' signing key is on GitHub @{}.",
                    key.login
                ));
                self.state.pending_key = None;
                if let Err(error) = self.save() {
                    self.actions
                        .push(format!("Could not save onboarding state: {error}"));
                }
            }
            Err(reason) => {
                self.say(&format!(
                    "  Could not add your agents' signing key to GitHub: {reason}."
                ));
                self.actions.push(format!(
                    "Add your agents' signing key to GitHub @{}: Settings, SSH and GPG keys, New SSH key, Key type Signing Key, paste: {} (or fix gh and run buzz onboard again).",
                    key.login, key.public
                ));
            }
        }
    }

    /// Step 8. How to pair Paseo, how to use workflows and the Buzz app,
    /// and everything still to do.
    pub fn next_steps(&mut self, pairing: Option<&Pairing>) {
        match pairing {
            Some(pairing) => {
                self.say("  Paseo: your daemon is provisioned. Pair a Paseo app with it:");
                self.say(&format!("    Password:  {}", pairing.password.expose()));
                self.say(&format!("    Server ID: {}", pairing.server_id));
                self.say(&format!("    Offer:     {}", pairing.offer_url));
                match paseo::qr(&pairing.offer_url) {
                    Ok(code) => self.say(&code),
                    Err(error) => self.say(&format!("    {error}")),
                }
                self.say("    Paseo desktop: open the offer link on that computer, then enter the password.");
                self.say("    Paseo web: open the offer link in a browser (app.paseo.sh), then enter the password.");
                self.say("    Paseo mobile: scan the QR code with your phone's camera or the Paseo app, then enter the password.");
                self.say("    The password is not saved. Run buzz onboard again to show it.");
                self.say("    Your daemon lists an agent after its first start in a channel thread; agents added later appear after the daemon restarts.");
            }
            None => self.say(
                "  Paseo isn't provisioned for you yet. Run buzz onboard again after an admin enables Paseo daemons.",
            ),
        }
        let channel = self
            .state
            .channel
            .as_ref()
            .map_or_else(String::new, |channel| format!(" in #{}", channel.name));
        let channel_id = self
            .state
            .channel
            .as_ref()
            .map_or("<channel id>", |channel| channel.id.as_str());
        self.say(&format!(
            "  Workflows: BUZZ_PRIVATE_KEY=\"$(cat {})\" BUZZ_RELAY_URL={} buzz workflows list --channel {channel_id} (see buzz workflows --help).",
            self.store.identity_path().display(),
            self.relay_url
        ));
        self.say(&format!(
            "  Buzz app instead: install the Voltage build (voltage/remote-agents.md), sign in with the key in {}, and join {}. Your agents appear under Agents; mention one{channel} to start a session.",
            self.store.identity_path().display(),
            self.relay_url
        ));
        if !self.actions.is_empty() {
            self.say("  Action needed:");
            let actions = self.actions.clone();
            for action in actions {
                self.say(&format!("    - {action}"));
            }
        }
    }
}
