//! `buzz paseo setup`: onboard a developer to their Paseo daemon from the CLI.
//!
//! 1. Ask the manager for the stored profile (`profile`).
//! 2. Collect only what is missing: a complete profile for a new developer, a
//!    Claude subscription token if the assistant has none, replacements with
//!    `--update`, and the optional Paseo GitHub read token when the daemon is
//!    first created.
//! 3. Enroll when anything changed or no daemon exists yet. The manager keeps
//!    stored values that the request leaves out, and creates the daemon.
//! 4. Grant the daemon's gateway key for 90 days.
//! 5. Show the pairing offer as a QR code, with the password on stderr only.

use buzz_session_control::Op;
use nostr::PublicKey;
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use super::profile::{self, Collected, Local, Prompt};
use super::{pairing, Manager, ENROLL_BUDGET, PROFILE_BUDGET, REFUSED};
use crate::client::BuzzClient;
use crate::error::CliError;

pub(super) struct Options {
    pub developer: Option<String>,
    pub local: Local,
    pub update: bool,
    /// Replace the agents' signing key (implies an update).
    pub replace_signing_key: bool,
    pub github_api: String,
    /// Create the agents' signing key (`ssh-keygen` in production).
    pub keygen: profile::Keygen,
    /// Add its public half to GitHub (`gh` in production).
    pub add_key: fn(&mut dyn Prompt, &str) -> bool,
    /// Channels to keep the gateway out of.
    pub exclude: Vec<String>,
    /// The team channel a later import creates, which an exclusion may name
    /// before it exists.
    pub pending_channel: Option<String>,
}

pub(super) struct Outcome {
    /// Public JSON for stdout: no credential or password.
    pub report: Value,
    pub offer_url: String,
    pub password: Zeroizing<String>,
}

pub(super) async fn run(
    client: &BuzzClient,
    manager: &Manager,
    prompt: &mut dyn Prompt,
    options: Options,
) -> Result<Outcome, CliError> {
    let described = manager.call(Op::Profile, json!({}), PROFILE_BUDGET).await?;
    if described["paseo_enabled"] == false {
        return Err(CliError::Other(
            "Paseo is not enabled on this Sandbox manager; ask an operator to enable it".into(),
        ));
    }
    let developer = described["developer"]
        .as_str()
        .map(str::to_owned)
        .or(options.developer)
        .ok_or_else(|| {
            CliError::Usage(
                "this Sandbox manager does not report your developer ID; rerun with \
                 --developer <id> (your key in the Terraform `developers` map)"
                    .into(),
            )
        })?;
    let mut base = Map::new();
    base.insert("version".into(), json!(1));
    base.insert(
        "owner_pubkey".into(),
        json!(client.keys().public_key().to_hex()),
    );
    base.insert("developer".into(), json!(developer));

    let stored = &described["profile"];
    let collected = if stored.is_null() {
        Some(
            profile::new_profile(
                prompt,
                base.clone(),
                &options.local,
                &options.github_api,
                options.keygen,
            )
            .await?,
        )
    } else if options.update || options.replace_signing_key {
        let keygen = options.replace_signing_key.then_some(options.keygen);
        Some(
            profile::updated_profile(
                prompt,
                base.clone(),
                &options.github_api,
                &options.local,
                keygen,
            )
            .await?,
        )
    } else if !has_model(stored, "claude_oauth_token") {
        // The daemon's assistant receives only the subscription token.
        profile::assistant_token(prompt, base.clone(), has_model(stored, "anthropic_api_key"))?
    } else {
        None
    };
    let has_daemon = described.get("paseo").is_some_and(Value::is_object);
    let git_token = if !has_daemon || options.update {
        prompt.note(profile::PASEO_GIT_HELP);
        let token = prompt.secret("Paseo GitHub read token (Enter to skip)")?;
        if token
            .as_deref()
            .is_some_and(|token| !token.starts_with("github_pat_"))
        {
            return Err(CliError::Usage(
                "the Paseo GitHub read token must be a fine-grained token (github_pat_...)".into(),
            ));
        }
        token
    } else {
        None
    };

    let mut enrolled = false;
    let response = if collected.is_some() || git_token.is_some() || !has_daemon {
        let Collected {
            profile,
            signing_public,
        } = collected.unwrap_or(Collected {
            profile: Value::Object(base),
            signing_public: None,
        });
        let mut payload = json!({ "profile": profile });
        if let Some(token) = git_token {
            payload["paseo"] = json!({ "github_read_token": token });
        }
        let response = manager
            .call(Op::Enroll, payload, ENROLL_BUDGET)
            .await
            .map_err(|error| match (&error, &signing_public) {
                // A lost reply may hide a stored profile whose new key never
                // reached GitHub; a replacement key recovers either way.
                (CliError::Other(text), Some(_)) if !text.starts_with(REFUSED) => {
                    CliError::Other(format!(
                        "{text}. If the manager stored your profile anyway, run \
                     `buzz paseo setup --replace-signing-key`"
                    ))
                }
                _ => error,
            })?;
        enrolled = true;
        if let Some(public) = signing_public {
            (options.add_key)(prompt, &public);
        }
        response
    } else {
        described
    };
    let (public, password) = pairing(&response).map_err(|_| {
        CliError::Other(
            "the Sandbox manager returned no Paseo pairing; rerun setup, or ask an operator \
             to check the daemon Secret"
                .into(),
        )
    })?;
    let gateway = public["gateway_pubkey"]
        .as_str()
        .and_then(|key| PublicKey::from_hex(key).ok())
        .ok_or_else(super::unavailable)?;
    let grant =
        super::super::gateway::grant(client, gateway, buzz_sdk::gateway::MAX_DELEGATION_SECS)
            .await?;
    let channels = super::channels::share(
        client,
        gateway,
        &options.exclude,
        options.pending_channel.as_deref(),
    )
    .await?;
    let offer_url = public["offer_url"].as_str().unwrap_or_default().to_owned();
    Ok(Outcome {
        report: json!({
            "developer": developer,
            "enrolled": enrolled,
            "gateway_pubkey": public["gateway_pubkey"],
            "server_id": public["server_id"],
            "offer_url": offer_url,
            "grant": serde_json::from_str::<Value>(&grant).unwrap_or(Value::String(grant)),
            "channels": channels,
        }),
        offer_url,
        password,
    })
}

fn has_model(summary: &Value, key: &str) -> bool {
    summary["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|model| model == key))
}

/// Show the pairing on the interactive terminal: QR code, link, password.
pub(super) fn show_pairing(outcome: &Outcome) {
    eprintln!("\nYour Paseo daemon is ready. In the Paseo app, scan this code or open the link:");
    if let Some(code) = profile::qr(&outcome.offer_url) {
        eprintln!("{code}");
    }
    eprintln!("{}", outcome.offer_url);
    super::print_password(&outcome.password);
    eprintln!(
        "Paseo saves the host without asking for the password: open the host's settings in \
         Paseo, enter the password, and connect."
    );
    let channels = &outcome.report["channels"];
    if channels["admitted"] == false {
        eprintln!(
            "Your daemon is not a community member yet, so it has no channels. Ask a relay admin to run:\n  {}\nthen rerun `buzz paseo setup`.",
            channels["admin_command"].as_str().unwrap_or_default()
        );
    } else {
        let count = |key: &str| channels[key].as_array().map_or(0, Vec::len);
        eprintln!(
            "Shared {} new channel(s) with your daemon ({} already shared, {} excluded).",
            count("added"),
            count("already_present"),
            count("excluded")
        );
    }
    eprintln!("Rerun `buzz paseo setup` after you join channels, and within 90 days to renew the daemon's access.\n");
}

/// Fail after reporting when any channel refused the gateway, so a rerun is
/// visibly needed.
pub(super) fn outcome(report: &Value) -> Result<(), CliError> {
    let failed = report["channels"]["failed"].as_array().map_or(0, Vec::len);
    if failed == 0 {
        return Ok(());
    }
    Err(CliError::Other(format!(
        "{failed} channel(s) could not be shared with your daemon; fix the cause and rerun `buzz paseo setup`"
    )))
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
