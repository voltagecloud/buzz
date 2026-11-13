//! Owner-only Paseo onboarding over the encrypted Sandbox manager operations.
use std::{io::IsTerminal, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use buzz_session_control::{build_request, open_reply, Op, Request, Status, KIND};
use buzz_ws_client::{NostrWsConnection, RelayMessage};
use nostr::{Keys, PublicKey, Timestamp};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::{client::BuzzClient, error::CliError};

#[derive(clap::Subcommand)]
pub(crate) enum PaseoCmd {
    /// Show public pairing details; print the password only to an interactive stderr
    Pairing {
        #[arg(long, env = "BUZZ_MANAGER_PUBKEY", hide_env_values = true)]
        manager: String,
    },
    /// Set up your Paseo daemon: profile, daemon, access grant, and pairing.
    ///
    /// Run it once to onboard; it asks only for what is missing and explains
    /// where to get each value. Run it again to renew the daemon's 90-day
    /// grant and show the pairing again, or with --update to replace stored
    /// credentials.
    Setup {
        /// The Sandbox manager's public key (hex)
        #[arg(long, env = "BUZZ_MANAGER_PUBKEY", hide_env_values = true)]
        manager: String,
        /// Your developer ID (the Terraform `developers` key), when the
        /// manager does not report it
        #[arg(long)]
        developer: Option<String>,
        /// A via profile file to enroll instead of the shared one
        #[arg(long, value_name = "FILE")]
        via_profile: Option<std::path::PathBuf>,
        /// Replace stored credentials; Enter keeps each one
        #[arg(long)]
        update: bool,
        /// Create a new signing key for your agents and add it to GitHub
        #[arg(long)]
        replace_signing_key: bool,
        /// A channel name or UUID to keep your daemon out of; repeatable
        #[arg(long = "exclude", value_name = "CHANNEL")]
        exclude: Vec<String>,
        /// Also create the personas of this team snapshot as remote agents
        /// (a `buzz-team-snapshot` export, such as `asgard.team.json`)
        #[arg(long, value_name = "FILE")]
        team: Option<std::path::PathBuf>,
    },
    /// Create a team snapshot's personas as remote agents in a private team channel.
    ///
    /// Each agent gets its own key, profile and listing, joins the team
    /// channel, and is started once there so later mentions launch it.
    /// Rerunning resumes; it never creates a second identity for a member.
    ImportTeam {
        /// A `buzz-team-snapshot` export, such as `asgard.team.json`
        file: std::path::PathBuf,
        /// The Sandbox manager's public key (hex)
        #[arg(long, env = "BUZZ_MANAGER_PUBKEY", hide_env_values = true)]
        manager: String,
        /// The team channel's name (default: the team's name)
        #[arg(long)]
        channel: Option<String>,
        /// The model for every agent (default: each harness's default)
        #[arg(long)]
        model: Option<String>,
    },
}

pub(crate) async fn dispatch(command: PaseoCmd, client: &BuzzClient) -> Result<(), CliError> {
    if client.auth_tag_owner_hex().is_some() {
        return Err(CliError::Auth(
            "Paseo pairing and setup require the owner's direct signing key".into(),
        ));
    }
    match command {
        PaseoCmd::Pairing { manager } => {
            require_terminal()?;
            let manager = Manager::new(client, &manager)?;
            let (public, password) =
                pairing(&manager.call(Op::Profile, json!({}), PROFILE_BUDGET).await?)?;
            print_password(&password);
            println!("{public}");
            Ok(())
        }
        PaseoCmd::ImportTeam {
            file,
            manager,
            channel,
            model,
        } => {
            let team = read_team(&file)?;
            let manager = Manager::new(client, &manager)?;
            let report =
                team::import(client, &manager, &team, team_options(channel, model)?).await?;
            println!("{report}");
            team::outcome(&report)
        }
        PaseoCmd::Setup {
            manager,
            developer,
            via_profile,
            update,
            replace_signing_key,
            exclude,
            team,
        } => {
            // Read the snapshot before any prompt, so a bad path fails first.
            let team = team.as_deref().map(read_team).transpose()?;
            require_terminal()?;
            if !std::io::stdin().is_terminal() {
                return Err(CliError::Usage(
                    "setup asks questions; run it from an interactive terminal".into(),
                ));
            }
            let via_profile = via_profile
                .map(|path| read_bounded(&path, MAX_VIA_PROFILE_BYTES))
                .transpose()?;
            let manager = Manager::new(client, &manager)?;
            let options = setup::Options {
                developer,
                local: profile::Local::detect(via_profile),
                update,
                github_api: "https://api.github.com".into(),
                keygen: profile::generate_signing_key,
                add_key: profile::add_signing_key,
                exclude: exclude.clone(),
                pending_channel: team.as_ref().map(|team| team.name.clone()),
                replace_signing_key,
            };
            let mut outcome = setup::run(client, &manager, &mut profile::Terminal, options).await?;
            // Pairing first: a failed agent start must not hide it.
            setup::show_pairing(&outcome);
            let mut team_result = Ok(());
            if let Some(team) = team {
                eprintln!(
                    "Creating the {} agents; each starts once, which can take a few minutes.",
                    team.name
                );
                match team::import(client, &manager, &team, team_options(None, None)?).await {
                    Ok(report) => {
                        team_result = team::outcome(&report);
                        outcome.report["team"] = report;
                        // The team channel did not exist when setup shared
                        // channels; share again so the daemon can use it.
                        if let Some(gateway) = outcome.report["gateway_pubkey"]
                            .as_str()
                            .and_then(|key| PublicKey::from_hex(key).ok())
                        {
                            outcome.report["channels"] =
                                channels::share(client, gateway, &exclude, None).await?;
                        }
                    }
                    Err(error) => team_result = Err(error),
                }
            }
            println!("{}", outcome.report);
            setup::outcome(&outcome.report).and(team_result)
        }
    }
}

/// Snapshots embed avatars; Asgard's seven are about 2 MB.
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
/// The manager stores the whole profile in 64 KiB.
const MAX_VIA_PROFILE_BYTES: u64 = 64 * 1024;

fn read_team(path: &std::path::Path) -> Result<team::Team, CliError> {
    team::parse_snapshot(&read_bounded(path, MAX_SNAPSHOT_BYTES)?)
}

/// Read at most `max` bytes of UTF-8, refusing larger input (or a device
/// that never ends) instead of exhausting memory.
fn read_bounded(path: &std::path::Path, max: u64) -> Result<String, CliError> {
    use std::io::Read;
    let failed = |error: std::io::Error| {
        CliError::Usage(format!("could not read {}: {error}", path.display()))
    };
    let file = std::fs::File::open(path).map_err(failed)?;
    let mut text = String::new();
    file.take(max + 1)
        .read_to_string(&mut text)
        .map_err(failed)?;
    if text.len() as u64 > max {
        return Err(CliError::Usage(format!(
            "{} is larger than {max} bytes",
            path.display()
        )));
    }
    Ok(text)
}

fn team_options(channel: Option<String>, model: Option<String>) -> Result<team::Options, CliError> {
    let journal_dir = dirs::config_dir()
        .ok_or_else(|| CliError::Other("no configuration directory for the team journal".into()))?
        .join("buzz")
        .join("paseo-teams");
    Ok(team::Options {
        channel,
        model,
        journal_dir,
    })
}

const PROFILE_BUDGET: Duration = Duration::from_secs(15);
/// `enroll` checks the GitHub token and writes two secrets before it answers.
const ENROLL_BUDGET: Duration = Duration::from_secs(90);

fn require_terminal() -> Result<(), CliError> {
    if std::io::stderr().is_terminal() {
        return Ok(());
    }
    Err(CliError::Usage("pairing requires interactive stderr; its password is never written to JSON or redirected output".into()))
}

fn print_password(password: &str) {
    eprintln!("Paseo pairing password (private; interactive stderr only): {password}");
}

/// The owner's encrypted request channel to one Sandbox manager.
pub(super) struct Manager {
    relay: String,
    owner: Keys,
    manager: PublicKey,
}

impl Manager {
    fn new(client: &BuzzClient, manager: &str) -> Result<Self, CliError> {
        let manager = PublicKey::from_hex(manager)
            .map_err(|_| CliError::Usage("invalid manager public key".into()))?;
        let relay = client.relay_url().replacen("https://", "wss://", 1);
        if !relay.starts_with("wss://") {
            return Err(CliError::Usage("pairing requires a WSS relay".into()));
        }
        Ok(Self {
            relay,
            owner: client.keys().clone(),
            manager,
        })
    }

    /// Send one owner-signed operation and return the manager's response.
    /// A refusal (`ok: false`) is returned as an error naming its code.
    pub(super) async fn call(
        &self,
        op: Op,
        payload: Value,
        budget: Duration,
    ) -> Result<Value, CliError> {
        let request = Request::new(op, uuid::Uuid::new_v4().to_string(), payload);
        refusal(
            call(
                &self.relay,
                &self.owner,
                None,
                self.manager,
                request,
                budget,
            )
            .await?,
        )
    }

    /// Send one operation signed as an owned agent, admitted through its
    /// NIP-OA `auth_tag`, as the relay provider does for `deploy`.
    pub(super) async fn call_as(
        &self,
        agent: &Keys,
        auth_tag: &str,
        op: Op,
        request_id: &str,
        payload: Value,
        budget: Duration,
    ) -> Result<Value, CliError> {
        let request = Request::new(op, request_id.to_owned(), payload);
        refusal(
            call(
                &self.relay,
                agent,
                Some(auth_tag),
                self.manager,
                request,
                budget,
            )
            .await?,
        )
    }

    pub(super) fn relay(&self) -> &str {
        &self.relay
    }
}

/// The manager's response, or its refusal (`ok: false`) as an error naming
/// the code.
fn refusal(response: Value) -> Result<Value, CliError> {
    if response["ok"] == true {
        return Ok(response);
    }
    let text = |key: &str| response[key].as_str().unwrap_or("unknown").to_owned();
    Err(CliError::Other(format!(
        "{REFUSED} ({}): {}",
        text("code"),
        text("error")
    )))
}

/// How a definite manager refusal begins, unlike an ambiguous lost reply.
const REFUSED: &str = "the Sandbox manager refused the request";

fn unavailable() -> CliError {
    CliError::Other(
        "Paseo pairing unavailable; check manager, enrollment and relay, then retry".into(),
    )
}

async fn call(
    relay: &str,
    owner: &Keys,
    auth_tag: Option<&str>,
    manager: PublicKey,
    request: Request,
    budget: Duration,
) -> Result<Value, CliError> {
    tokio::time::timeout(budget, async {
        let event = build_request(owner, &manager, auth_tag, &request, Timestamp::now()).map_err(|_| unavailable())?;
        let tag = auth_tag.map(buzz_sdk::nip_oa::parse_auth_tag).transpose().map_err(|_| unavailable())?;
        let mut ws = NostrWsConnection::connect_authenticated(relay, owner, tag.as_ref()).await.map_err(|_| unavailable())?;
        ws.send_raw(&json!(["REQ","paseo-pairing",{"kinds":[KIND],"authors":[manager],"#p":[owner.public_key()],"#e":[event.id],"limit":0}])).await.map_err(|_| unavailable())?;
        let (mut sent, mut accepted, mut done) = (false, false, None);
        for _ in 0..64 {
            match ws.next_event(budget).await.map_err(|_| unavailable())? {
                RelayMessage::Eose { subscription_id } if subscription_id == "paseo-pairing" && !sent => {
                    ws.send_raw(&json!(["EVENT",event])).await.map_err(|_| unavailable())?;
                    sent = true;
                }
                RelayMessage::Ok(ok) if sent && ok.event_id == event.id.to_hex() => {
                    if !ok.accepted { return Err(unavailable()); }
                    accepted = true;
                }
                RelayMessage::Event { subscription_id, event: reply } if sent && subscription_id == "paseo-pairing" => {
                    if reply.content.len() > 100_000 { continue; }
                    let Ok(opened) = open_reply(&reply, owner, &manager, &event.id) else { continue };
                    if opened.request_id != request.request_id { continue; }
                    let now = Timestamp::now();
                    if buzz_session_control::check_created_at(reply.created_at, now).is_err()
                        || !reply.tags.iter().any(|tag| matches!(tag.as_slice(), [name, expiry] if name == "expiration" && expiry.parse::<u64>().is_ok_and(|expiry| expiry > now.as_secs()))) {
                        continue;
                    }
                    if opened.status == Status::Done { done = Some(opened.response.ok_or_else(unavailable)?); }
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(unavailable()),
                _ => {}
            }
            if accepted && done.is_some() { return done.ok_or_else(unavailable); }
        }
        Err(unavailable())
    }).await.map_err(|_| unavailable())?
}

/// The profile operation's pairing, for the existing test vectors.
#[cfg(test)]
async fn exchange(
    relay: &str,
    owner: &Keys,
    manager: PublicKey,
    budget: Duration,
) -> Result<(Value, Zeroizing<String>), CliError> {
    let request = Request::new(Op::Profile, uuid::Uuid::new_v4().to_string(), json!({}));
    pairing(&call(relay, owner, None, manager, request, budget).await?)
}

fn pairing(response: &Value) -> Result<(Value, Zeroizing<String>), CliError> {
    if response["ok"] != true {
        return Err(unavailable());
    }
    let value = &response["paseo"];
    let text = |key| {
        value[key]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control))
            .ok_or_else(unavailable)
    };
    let password = text("password")?;
    if !URL_SAFE_NO_PAD
        .decode(password)
        .is_ok_and(|raw| raw.len() == 32)
    {
        return Err(unavailable());
    }
    let gateway = text("gateway_pubkey")?;
    PublicKey::from_hex(gateway).map_err(|_| unavailable())?;
    Ok((
        json!({"offer_url":text("offer_url")?,"server_id":text("server_id")?,"gateway_pubkey":gateway}),
        Zeroizing::new(password.to_owned()),
    ))
}

mod channels;
mod profile;
mod setup;
mod team;
#[cfg(test)]
mod test_prompt;

#[cfg(test)]
mod tests;
