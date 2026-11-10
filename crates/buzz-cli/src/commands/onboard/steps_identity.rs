//! Step 1: the owner's Buzz identity, community membership and profile,
//! and the manager's view of what is already enrolled.

use buzz_session_control::Op;
use nostr::{EventBuilder, Keys, Kind, ToBech32};
use serde_json::{json, Value};

use super::io::Prompter;
use super::manager::{self, ControlTransport};
use super::paseo::{self, Pairing};
use super::profile::{self, Summary};
use super::relay::{Claimed, Relay, Signer};
use super::state::Secret;
use super::tools::Tools;
use super::Wizard;
use crate::error::CliError;

/// What the manager holds for this owner before anything is sent.
#[derive(Default)]
pub struct OnFile {
    /// The stored profile, `None` when nothing is enrolled.
    pub summary: Option<Summary>,
    /// The Paseo pairing the manager returned with it.
    pub pairing: Option<Pairing>,
    /// The manager can provision Paseo, even if its Secret is absent.
    pub paseo_enabled: bool,
}

/// The code in an invite code or an invite URL (`…/invite/<code>`), checked
/// as the Buzz app checks it: `^[a-zA-Z0-9._-]{1,256}$`.
pub fn invite_code(text: &str) -> Result<String, CliError> {
    let text = text.trim();
    let code = match url::Url::parse(text) {
        Ok(url) => url
            .path_segments()
            .and_then(|mut segments| {
                segments.find(|segment| *segment == "invite")?;
                segments.next().map(str::to_owned)
            })
            .unwrap_or_default(),
        Err(_) => text.to_owned(),
    };
    let valid = (1..=256).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(code)
    } else {
        Err(CliError::Usage("That invite code is not valid.".into()))
    }
}

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Step 1. Loads, imports or generates the owner key (saved mode
    /// `0600`, never printed), joins the community with an invite when the
    /// key is not a member, and publishes a profile when there is none.
    pub async fn identity(&mut self) -> Result<Keys, CliError> {
        let keys = self.owner_keys()?;
        let npub = keys
            .public_key()
            .to_bech32()
            .map_err(|_| CliError::Key("could not encode the public key".into()))?;
        self.say(&format!(
            "  Identity {npub} ({} hex), key in {}",
            keys.public_key().to_hex(),
            self.store.identity_path().display()
        ));
        if !self.state.joined {
            self.join(&keys).await?;
        }
        if !self.state.owner_profile {
            self.owner_profile(&keys).await?;
        }
        Ok(keys)
    }

    fn owner_keys(&mut self) -> Result<Keys, CliError> {
        if let Some(saved) = self.store.load_identity().map_err(CliError::Other)? {
            let keys = Keys::parse(saved.expose()).map_err(|_| {
                CliError::Key(format!(
                    "{} does not hold a valid key",
                    self.store.identity_path().display()
                ))
            })?;
            let hex = keys.public_key().to_hex();
            if !self.state.owner_pubkey.is_empty() && self.state.owner_pubkey != hex {
                return Err(CliError::Key(
                    "the saved key does not match the saved onboarding state".into(),
                ));
            }
            if self.state.owner_pubkey.is_empty() {
                self.state.owner_pubkey = hex;
                self.save()?;
            }
            return Ok(keys);
        }
        let import = self.args.import_key
            || (self.prompter.interactive()
                && self
                    .prompter
                    .ask(
                        "  Generate a new Buzz key, or import the one you use in the Buzz app? (generate/import)",
                        Some("generate"),
                        "",
                    )?
                    .trim()
                    .eq_ignore_ascii_case("import"));
        let keys = if import {
            let text = match self.env.private_key.take() {
                Some(text) => text,
                None => self.prompter.secret(
                    "  Your Buzz key (nsec or hex)",
                    "set BUZZ_PRIVATE_KEY with --import-key",
                )?,
            };
            Keys::parse(text.trim())
                .map_err(|_| CliError::Key("that Buzz key is not valid".into()))?
        } else {
            Keys::generate()
        };
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|_| CliError::Key("could not encode the key".into()))?;
        self.store
            .save_identity(&Secret::new(nsec))
            .map_err(CliError::Other)?;
        self.state.owner_pubkey = keys.public_key().to_hex();
        self.save()?;
        self.say(if import {
            "  Imported your key."
        } else {
            "  Generated a new key. To be added as a developer, give your admin the hex key on the next line."
        });
        Ok(keys)
    }

    async fn join(&mut self, keys: &Keys) -> Result<(), CliError> {
        if self.relay.is_member(keys).await? {
            self.state.joined = true;
            return self.save();
        }
        let answer = match self.args.invite.clone() {
            Some(invite) => invite,
            None => self.prompter.ask(
                "  You are not a member yet. Invite code or link",
                None,
                "pass --invite or set BUZZ_ONBOARD_INVITE",
            )?,
        };
        let code = invite_code(&answer)?;
        let receipt = match self.relay.join_policy().await? {
            None => None,
            Some(policy) => {
                let base = crate::client::normalize_relay_url(&self.relay_url);
                self.say(&format!(
                    "  This community has terms ({base}/api/join-policy/terms) and a privacy policy ({base}/api/join-policy/privacy)."
                ));
                let accepted = self.args.accept_join_policy
                    || self.prompter.confirm("  Accept them?", false)?;
                if !accepted {
                    return Err(CliError::Usage(
                        "Joining requires accepting the join policy (--accept-join-policy).".into(),
                    ));
                }
                let age = !policy.age_attestation_required
                    || self.args.confirm_age
                    || self
                        .prompter
                        .confirm("  Do you meet the community's minimum age?", false)?;
                if !age {
                    return Err(CliError::Usage(
                        "Joining requires confirming the minimum age (--confirm-age).".into(),
                    ));
                }
                Some(
                    self.relay
                        .accept_policy(keys, &code, &policy, policy.age_attestation_required)
                        .await?,
                )
            }
        };
        let claimed = self.relay.claim(keys, &code, receipt.as_deref()).await?;
        self.say(match claimed {
            Claimed::Joined => "  Joined the community.",
            Claimed::AlreadyMember => "  Already a member of the community.",
        });
        self.state.joined = true;
        self.save()
    }

    async fn owner_profile(&mut self, keys: &Keys) -> Result<(), CliError> {
        let filter = json!({"kinds": [0], "authors": [keys.public_key().to_hex()], "limit": 1});
        if self.relay.query(keys, filter).await?.is_empty() {
            let name = match self.args.name.clone() {
                Some(name) => name,
                None => self
                    .prompter
                    .ask("  Your display name", None, "pass --name")?,
            };
            let name = name.trim().to_owned();
            if !(1..=100).contains(&name.chars().count()) {
                return Err(CliError::Usage(
                    "The display name must be 1 to 100 characters.".into(),
                ));
            }
            let content = json!({"name": name, "display_name": name}).to_string();
            let event = EventBuilder::new(Kind::Metadata, content)
                .sign_with_keys(keys)
                .map_err(|error| CliError::Other(format!("signing failed: {error}")))?;
            self.relay.publish(&Signer::owner(keys), event).await?;
            self.say(&format!("  Published your profile as {name}."));
        }
        self.state.owner_profile = true;
        self.save()
    }

    /// What the manager holds for this owner (the `profile` request), and
    /// its Paseo pairing. A refusal stops the run: an owner the manager does
    /// not know cannot enroll, and an unknown state cannot pick between a
    /// complete and a partial profile.
    pub async fn on_file(&mut self, keys: &Keys) -> Result<OnFile, CliError> {
        let response = manager::ask(
            &self.transport,
            &self.manager,
            &Signer::owner(keys),
            Op::Profile,
            json!({}),
            manager::OWNER_TIMEOUT,
        )
        .await
        .map_err(|error| {
            CliError::Other(format!(
                "Could not check what is enrolled: {error}. Run buzz onboard again."
            ))
        })?;
        let summary = profile::on_file_reply(&response).map_err(|text| {
            if response.get("code").and_then(Value::as_str) == Some("unauthorized_owner") {
                CliError::Auth(format!(
                    "{text} Your owner key is {}.",
                    keys.public_key().to_hex()
                ))
            } else {
                CliError::Other(text)
            }
        })?;
        let pairing = match paseo::pairing(&response) {
            Ok(pairing) => pairing,
            Err(error) => {
                self.actions.push(error);
                None
            }
        };
        match &summary {
            Some(summary) => {
                self.state.enrolled_version = Some(summary.version_id.clone());
                self.save()?;
                let text = summary.text();
                self.say(&format!("  {text}"));
            }
            None => self.say("  Nothing enrolled yet."),
        }
        let paseo_enabled = response
            .get("paseo_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(pairing.is_some());
        Ok(OnFile {
            summary,
            pairing,
            paseo_enabled,
        })
    }
}
