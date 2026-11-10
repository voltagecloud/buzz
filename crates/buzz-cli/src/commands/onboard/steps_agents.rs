//! Step 5 (agents and their channel) and the first start that registers
//! each new agent with the Sandbox manager.

use std::collections::HashSet;

use buzz_session_control::Op;
use nostr::Keys;
use serde_json::{json, Value};
use uuid::Uuid;

use super::agents::{self, LISTING_KIND, PRESETS};
use super::io::Prompter;
use super::manager::{self, ControlTransport};
use super::relay::{Relay, Signer};
use super::state::ChannelRef;
use super::tools::Tools;
use super::Wizard;
use crate::error::CliError;

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Step 5. Creates agents from presets, each with its own key the owner
    /// attests, and publishes what lists it in every Buzz app: its kind:0
    /// profile, the owner's kind 30177 record, and its `bot` membership in
    /// the workspace channel. Agents the Buzz app already lists are shown and
    /// never duplicated. Each write is saved as it lands, so a re-run
    /// finishes only what is missing.
    pub async fn agents(&mut self, keys: &Keys, login: &str) -> Result<(), CliError> {
        let owner = keys.public_key().to_hex();
        let filter = json!({"kinds": [LISTING_KIND], "authors": [owner], "limit": 500});
        let events = self.relay.query(keys, filter).await?;
        let ours: HashSet<String> = self.state.agents.iter().map(|a| a.pubkey.clone()).collect();
        let others: Vec<(String, String)> = agents::remote_listings(&events, &owner)
            .into_iter()
            .filter(|(pubkey, _)| !ours.contains(pubkey))
            .collect();
        if !others.is_empty() {
            let names: Vec<_> = others.iter().map(|(_, name)| name.as_str()).collect();
            self.say(&format!(
                "  Already in your Buzz app: {}. They keep running from the app.",
                names.join(", ")
            ));
        }
        for preset in self.wanted_presets(!others.is_empty())? {
            if self
                .state
                .agents
                .iter()
                .any(|agent| agent.preset == preset.id)
            {
                continue;
            }
            if others
                .iter()
                .any(|(_, name)| name.eq_ignore_ascii_case(preset.name))
            {
                self.say(&format!(
                    "  Skipped {}: an agent with that name is already listed.",
                    preset.name
                ));
                continue;
            }
            if !agents::credential_available(preset.needs, &self.available_models) {
                self.actions.push(format!(
                    "{} runs {} and needs {}: add it with buzz onboard --reenroll.",
                    preset.name, preset.runtime, preset.needs
                ));
            }
            // Saved before anything is published, so its key is never lost.
            self.state.agents.push(agents::create(preset, keys)?);
            self.save()?;
        }
        if self.state.agents.is_empty() {
            self.say("  No agents to set up.");
            return Ok(());
        }
        let channel = self.channel(keys, login).await?;
        let channel_id = Uuid::parse_str(&channel.id)
            .map_err(|_| CliError::Other("the saved channel ID is invalid".into()))?;
        for index in 0..self.state.agents.len() {
            let agent = self.state.agents[index].clone();
            if !agent.profile_published {
                let signer = Signer {
                    keys: agents::keys(&agent)?,
                    auth_tag: Some(agent.auth_tag.clone()),
                };
                self.relay
                    .publish(&signer, agents::profile_event(&agent)?)
                    .await?;
                self.state.agents[index].profile_published = true;
                self.save()?;
            }
            if !agent.listed {
                let event = agents::listing_event(&agent, keys)?;
                self.relay.publish(&Signer::owner(keys), event).await?;
                self.state.agents[index].listed = true;
                self.save()?;
            }
            if !agent.member {
                let event = agents::member_event(&agent, channel_id, keys)?;
                self.relay.publish(&Signer::owner(keys), event).await?;
                self.state.agents[index].member = true;
                self.save()?;
            }
            self.say(&format!(
                "  {} ({}): listed, and in #{}.",
                agent.name,
                agents::RUNTIMES
                    .iter()
                    .find(|runtime| runtime.id == agent.runtime)
                    .map_or(agent.runtime.as_str(), |runtime| runtime.label),
                channel.name
            ));
        }
        Ok(())
    }

    /// The presets to create: `--agents`, nothing more on a re-run, or the
    /// developer's choice (Brokkr unless the Buzz app already lists agents).
    fn wanted_presets(
        &mut self,
        has_others: bool,
    ) -> Result<Vec<&'static agents::Preset>, CliError> {
        let answer = if !self.args.agents.is_empty() {
            self.args.agents.join(",")
        } else if !self.state.agents.is_empty() {
            return Ok(Vec::new());
        } else {
            let default = if has_others { "none" } else { "brokkr" };
            if self.prompter.interactive() {
                for preset in PRESETS {
                    let line = format!(
                        "    {:<10} {:<8} {:<11} effort {:<6} {}",
                        preset.id,
                        preset.name,
                        preset.runtime,
                        if preset.effort.is_empty() {
                            "-"
                        } else {
                            preset.effort
                        },
                        if preset.model.is_empty() {
                            "(harness default model)"
                        } else {
                            preset.model
                        },
                    );
                    self.say(&line);
                }
            }
            self.prompter.ask(
                "  Agents to create (comma-separated presets, or none)",
                Some(default),
                "pass --agents",
            )?
        };
        let mut presets = Vec::new();
        for id in answer.split(',').map(str::trim).filter(|id| !id.is_empty()) {
            if id.eq_ignore_ascii_case("none") {
                continue;
            }
            let preset = agents::preset(id).ok_or_else(|| {
                let ids: Vec<_> = PRESETS.iter().map(|preset| preset.id).collect();
                CliError::Usage(format!(
                    "Unknown agent preset {id:?}; choose from {}.",
                    ids.join(", ")
                ))
            })?;
            if !presets
                .iter()
                .any(|known: &&agents::Preset| known.id == preset.id)
            {
                presets.push(preset);
            }
        }
        Ok(presets)
    }

    /// The workspace channel: the saved one, `--channel` (a UUID, or the
    /// name of one of the owner's channels), or a new private stream channel.
    async fn channel(&mut self, keys: &Keys, login: &str) -> Result<ChannelRef, CliError> {
        if let Some(channel) = &self.state.channel {
            return Ok(channel.clone());
        }
        let default = if login.is_empty() {
            format!("agents-{}", &self.state.owner_pubkey[..8])
        } else {
            format!("{}-agents", login.to_ascii_lowercase())
        };
        let wanted = match self.args.channel.clone() {
            Some(channel) => channel,
            None => self.prompter.ask(
                "  Workspace channel for your agents (a name or UUID)",
                Some(&default),
                "pass --channel",
            )?,
        };
        let wanted = buzz_core::channel::canonical_channel_name(&wanted).to_owned();
        let channel = match Uuid::parse_str(&wanted) {
            Ok(id) => {
                let filter = json!({"kinds": [39000], "#d": [id.to_string()], "limit": 1});
                let found = self.relay.query(keys, filter).await?;
                let event = found.first().ok_or_else(|| {
                    CliError::NotFound(format!("No channel {id} is visible to you."))
                })?;
                ChannelRef {
                    id: id.to_string(),
                    name: crate::client::extract_tag_value(event, "name"),
                }
            }
            Err(_) => match self.own_channel(keys, &wanted).await? {
                Some(channel) => channel,
                None => self.create_channel(keys, &wanted).await?,
            },
        };
        self.say(&format!(
            "  Workspace channel #{} ({}).",
            channel.name, channel.id
        ));
        self.state.channel = Some(channel.clone());
        self.save()?;
        Ok(channel)
    }

    /// One of the owner's channels named `name`.
    async fn own_channel(&self, keys: &Keys, name: &str) -> Result<Option<ChannelRef>, CliError> {
        let filter = json!({"kinds": [39002], "#p": [keys.public_key().to_hex()], "limit": 500});
        let ids: Vec<String> = self
            .relay
            .query(keys, filter)
            .await?
            .iter()
            .map(crate::client::extract_d_tag)
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Ok(None);
        }
        let filter = json!({"kinds": [39000], "#d": ids, "limit": 500});
        Ok(self
            .relay
            .query(keys, filter)
            .await?
            .iter()
            .find(|event| {
                crate::client::extract_tag_value(event, "name").eq_ignore_ascii_case(name)
            })
            .map(|event| ChannelRef {
                id: crate::client::extract_d_tag(event),
                name: crate::client::extract_tag_value(event, "name"),
            }))
    }

    async fn create_channel(&self, keys: &Keys, name: &str) -> Result<ChannelRef, CliError> {
        let id = Uuid::new_v4();
        let event = buzz_sdk::build_create_channel(
            id,
            name,
            Some(buzz_sdk::Visibility::Private),
            Some(buzz_sdk::ChannelKind::Stream),
            Some("Remote workspace agents set up with buzz onboard"),
            None,
            false,
        )
        .map_err(|error| CliError::Usage(format!("That channel name is not valid: {error}")))?
        .sign_with_keys(keys)
        .map_err(|error| CliError::Other(format!("signing failed: {error}")))?;
        self.relay.publish(&Signer::owner(keys), event).await?;
        Ok(ChannelRef {
            id: id.to_string(),
            name: name.to_owned(),
        })
    }

    /// Starts each agent the manager has not registered yet, once, as the
    /// Buzz app's host starts an agent for a mention: the owner mentions it
    /// in a new thread of the workspace channel, and the agent signs a
    /// `deploy` for that thread through the relay. On success the manager
    /// stores the agent's launch registration, so later mentions from any
    /// Buzz app or Paseo start it. Every failure is reported and stays
    /// pending for the next run.
    pub async fn first_start(&mut self, keys: &Keys) -> Result<(), CliError> {
        let pending: Vec<usize> = (0..self.state.agents.len())
            .filter(|index| !self.state.agents[*index].registered)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        if self.args.skip_first_start {
            for index in &pending {
                let name = self.state.agents[*index].name.clone();
                self.actions.push(format!(
                    "{name} is not registered with the Sandbox manager yet, so mentions do not start it and your Paseo daemon does not list it: run buzz onboard without --skip-first-start."
                ));
            }
            return Ok(());
        }
        if self.state.enrolled_version.is_none() {
            return Err(CliError::Other(
                "Enroll your profile before your agents can start.".into(),
            ));
        }
        let channel = self
            .state
            .channel
            .clone()
            .ok_or_else(|| CliError::Other("No workspace channel is saved.".into()))?;
        let channel_id = Uuid::parse_str(&channel.id)
            .map_err(|_| CliError::Other("the saved channel ID is invalid".into()))?;
        self.say(&format!(
            "  Starting {} agent(s) once in #{} to register them. The first start can take a few minutes.",
            pending.len(),
            channel.name
        ));
        let owner = keys.public_key().to_hex();
        let mut jobs = Vec::new();
        for index in pending {
            let agent = self.state.agents[index].clone();
            let kickoff = self.first_start_kickoff(index, channel_id, keys).await?;
            let payload = agents::deploy_payload(
                &agent,
                &self.relay_url,
                &owner,
                &channel.id,
                &kickoff.id.to_hex(),
                kickoff.created_at.as_secs(),
            );
            let signer = Signer {
                keys: agents::keys(&agent)?,
                auth_tag: Some(agent.auth_tag.clone()),
            };
            jobs.push((index, signer, payload));
        }
        let transport = &self.transport;
        let manager_key = self.manager;
        let results = futures_util::future::join_all(jobs.iter().map(|(_, signer, payload)| {
            manager::ask(
                transport,
                &manager_key,
                signer,
                Op::Deploy,
                payload.clone(),
                manager::DEPLOY_TIMEOUT,
            )
        }))
        .await;
        let mut failures = Vec::new();
        for ((index, _, _), result) in jobs.iter().zip(results) {
            let name = self.state.agents[*index].name.clone();
            match result {
                Ok(response) if response.get("ok").and_then(Value::as_bool) == Some(true) => {
                    self.state.agents[*index].registered = true;
                    self.save()?;
                    self.say(&format!("  {name} started and is registered."));
                }
                Ok(response) => {
                    failures.push(format!("{name}: {}", manager::refusal_text(&response)))
                }
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        for failure in &failures {
            self.say(&format!("  Did not start {failure}"));
        }
        Err(CliError::Other(format!(
            "{} agent(s) did not start: {}. Run buzz onboard again to retry.",
            failures.len(),
            failures.join("; ")
        )))
    }
}

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Resume one durable first-start identity across publication and deploy
    /// failures. A manager refusal can follow an already committed launch.
    async fn first_start_kickoff(
        &mut self,
        index: usize,
        channel: Uuid,
        keys: &Keys,
    ) -> Result<nostr::Event, CliError> {
        if let Some(event) = self.state.agents[index].kickoff.clone() {
            let found = self.relay.query(keys, json!({
                "kinds": [9], "ids": [event.id.to_hex()], "#h": [channel.to_string()], "limit": 1,
            })).await?;
            let mut seen = false;
            for value in found {
                let stored: nostr::Event = serde_json::from_value(value)
                    .map_err(|_| CliError::Other("Invalid first-start lookup response.".into()))?;
                if stored.id != event.id || stored.verify().is_err() {
                    return Err(CliError::Other(
                        "Unrelated first-start lookup response.".into(),
                    ));
                }
                seen = true;
            }
            if seen {
                if !self.state.agents[index].kickoff_published {
                    self.state.agents[index].kickoff_published = true;
                    self.save()?;
                }
                return Ok(event);
            }
            if self.state.agents[index].kickoff_published {
                return Err(CliError::Other(format!(
                    "The saved first-start thread {} is unavailable. Check that thread before retrying; a Sandbox may already be running.", event.id.to_hex()
                )));
            }
            // No deploy was attempted before the durable publication flag.
            // An unposted expired message can therefore be replaced safely.
            if nostr::Timestamp::now()
                .as_secs()
                .abs_diff(event.created_at.as_secs())
                <= 840
            {
                self.relay
                    .publish(&Signer::owner(keys), event.clone())
                    .await?;
                self.state.agents[index].kickoff_published = true;
                self.save()?;
                return Ok(event);
            }
        }
        let event = agents::kickoff_event(&self.state.agents[index], channel, keys)?;
        self.state.agents[index].kickoff = Some(event.clone());
        self.state.agents[index].kickoff_published = false;
        self.save()?;
        self.relay
            .publish(&Signer::owner(keys), event.clone())
            .await?;
        self.state.agents[index].kickoff_published = true;
        self.save()?;
        Ok(event)
    }
}
