//! `buzz paseo import-team`: create a `buzz-team-snapshot`'s personas as
//! Remote workspace agents, as the Buzz app does, without the app.
//!
//! For each member: a new agent key and the owner's NIP-OA attestation, the
//! agent's kind 0 profile, and the owner's kind 30177 listing. Then the agents
//! join a private team channel, and each gets one agent-signed `deploy` bound
//! to a welcome thread there. That deploy saves the manager registration that
//! later mentions launch from, in any channel the agent joins.
//!
//! Progress is journaled before every relay write, so a rerun resumes and
//! never mints a second identity for a member. The journal holds the agent
//! keys; like the app's headless key files, it is readable only by its owner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use buzz_session_control::Op;
use nostr::nips::nip19::ToBech32;
use nostr::{EventBuilder, Keys, Kind, PublicKey, Tag};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::Manager;
use crate::client::BuzzClient;
use crate::error::CliError;

/// The app's listing kind and runtime for Remote workspace agents.
const LISTING_KIND: u16 = 30177;
const REMOTE_RUNTIME: &str = "voltage.remote-workspace/sandbox";
/// The app migrates at most this many agents per batch.
const MAX_MEMBERS: usize = 32;
/// A deploy starts a Sandbox; the manager answers once it is running.
const DEPLOY_BUDGET: Duration = Duration::from_secs(600);
const MAX_AVATAR_BYTES: usize = 4 * 1024 * 1024;

/// A remote harness, as the app's Remote workspace runtimes define it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Harness {
    pub id: &'static str,
    command: &'static str,
    /// The variable carrying the startup model.
    model_env: &'static str,
    default_env: &'static [(&'static str, &'static str)],
}

const CLAUDE: Harness = Harness {
    id: "claude",
    command: "claude-agent-acp",
    model_env: "ANTHROPIC_MODEL",
    default_env: &[],
};
const CODEX: Harness = Harness {
    id: "codex",
    command: "codex-acp",
    model_env: "BUZZ_ACP_MODEL",
    default_env: &[],
};
const BUZZ_AGENT: Harness = Harness {
    id: "buzz-agent",
    command: "buzz-agent",
    model_env: "BUZZ_AGENT_MODEL",
    default_env: &[("MCP_HOOK_SERVERS", "*")],
};

/// The harness for a snapshot runtime (`codex-voltage`, `claude`, ...), or
/// Claude Code when the runtime names none of them.
pub(super) fn harness(runtime: &str) -> Harness {
    let runtime = runtime.to_ascii_lowercase();
    if runtime.contains("codex") {
        CODEX
    } else if runtime.contains("buzz-agent") {
        BUZZ_AGENT
    } else {
        CLAUDE
    }
}

/// One persona from the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Member {
    pub name: String,
    pub runtime: String,
    pub instructions: String,
    pub about: Option<String>,
    pub avatar: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Team {
    pub name: String,
    pub instructions: String,
    pub members: Vec<Member>,
}

/// Parse a `buzz-team-snapshot` export.
pub(super) fn parse_snapshot(text: &str) -> Result<Team, CliError> {
    let invalid = |why: &str| CliError::Usage(format!("not a Buzz team snapshot: {why}"));
    let value: Value = serde_json::from_str(text).map_err(|_| invalid("invalid JSON"))?;
    if value["format"] != "buzz-team-snapshot" || value["version"] != 1 {
        return Err(invalid("expected format buzz-team-snapshot, version 1"));
    }
    let text_at = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let name = text_at(&value, "/team/name").ok_or_else(|| invalid("the team has no name"))?;
    let members = value["members"]
        .as_array()
        .ok_or_else(|| invalid("no members"))?
        .iter()
        .map(|member| {
            let name = text_at(member, "/profile/displayName")
                .or_else(|| text_at(member, "/definition/name"))
                .ok_or_else(|| invalid("a member has no name"))?;
            let avatar = text_at(member, "/profile/avatarDataUrl")
                .map(|url| decode_avatar(&url))
                .transpose()?;
            Ok(Member {
                name,
                runtime: text_at(member, "/definition/runtime").unwrap_or_default(),
                instructions: text_at(member, "/definition/systemPrompt").unwrap_or_default(),
                about: text_at(member, "/profile/about"),
                avatar,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    if members.is_empty() || members.len() > MAX_MEMBERS {
        return Err(invalid(&format!("a team needs 1 to {MAX_MEMBERS} members")));
    }
    let mut names: Vec<String> = members
        .iter()
        .map(|member| member.name.to_lowercase())
        .collect();
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("two members share a name"));
    }
    Ok(Team {
        name,
        instructions: text_at(&value, "/team/instructions").unwrap_or_default(),
        members,
    })
}

fn decode_avatar(url: &str) -> Result<Vec<u8>, CliError> {
    let invalid = || CliError::Usage("not a Buzz team snapshot: invalid avatar".into());
    let data = url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(invalid)?;
    let bytes = STANDARD.decode(data).map_err(|_| invalid())?;
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}

/// The app's team section for agent `me`: the roster by name and npub, and
/// the team's instructions.
pub(super) fn team_prompt(team: &Team, roster: &[(String, PublicKey)], me: &PublicKey) -> String {
    let mut lines = vec![
        "<team>".to_owned(),
        format!(
            "You are on the team {}. Its members, you included:",
            Value::from(team.name.as_str())
        ),
    ];
    for (name, pubkey) in roster {
        let npub = pubkey.to_bech32().unwrap_or_else(|_| pubkey.to_hex());
        let you = if pubkey == me { ", you" } else { "" };
        lines.push(format!("- {name} ({npub}){you}"));
    }
    lines.push("Mention a teammate by name, with their npub, to reach them.".into());
    let instructions = team.instructions.trim();
    if !instructions.is_empty() {
        lines.push("<team-instructions>".into());
        lines.push(instructions.to_owned());
        lines.push("</team-instructions>".into());
    }
    lines.push("</team>".into());
    lines.join("\n")
}

/// The launch the app builds for a Remote workspace agent
/// (`remote_workspace::config::Config::spec`).
pub(super) fn launch(
    member: &Member,
    harness: Harness,
    model: Option<&str>,
    team_prompt: &str,
    owner: &PublicKey,
) -> Value {
    let mut policy_env: BTreeMap<String, String> = harness
        .default_env
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    let mut set = |key: &str, value: &str| policy_env.insert(key.to_owned(), value.to_owned());
    set("BUZZ_ACP_RELAY_OBSERVER", "true");
    set("BUZZ_ACP_LAZY_POOL", "true");
    set("BUZZ_ACP_AGENTS", "1");
    set("BUZZ_ACP_SESSION_POLICY", "thread");
    if !member.instructions.is_empty() {
        set("BUZZ_ACP_SYSTEM_PROMPT", &member.instructions);
    }
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        set(harness.model_env, model);
    }
    let title: String = member.name.chars().filter(|c| !c.is_control()).collect();
    set("BUZZ_ACP_SESSION_TITLE", title.trim());
    set("BUZZ_ACP_DISPLAY_NAME", title.trim());
    if !team_prompt.is_empty() {
        set("BUZZ_ACP_TEAM_INSTRUCTIONS", team_prompt);
    }
    json!({
        "command": harness.command,
        "args": [],
        "env": {},
        "policy_env": policy_env,
        "owner_pubkey": owner.to_hex(),
    })
}

/// One member's identity and progress. Saved before each relay write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Entry {
    pub name: String,
    pub pubkey: String,
    pub nsec: String,
    pub auth_tag: String,
    #[serde(default)]
    pub profile: bool,
    #[serde(default)]
    pub listed: bool,
    #[serde(default)]
    pub joined: bool,
    #[serde(default)]
    pub deployed: bool,
    /// The signed welcome its deploy is bound to, saved before it is posted,
    /// so a retry reuses the same thread instead of starting another.
    #[serde(default)]
    pub welcome: Option<Value>,
    /// The deploy's request ID, reused on retry so the manager replays its
    /// first answer instead of starting a second Sandbox.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct Journal {
    pub team: String,
    pub relay: String,
    pub owner: String,
    /// Chosen and saved before the channel is created.
    #[serde(default)]
    pub channel_id: Option<Uuid>,
    /// Whether the relay accepted the channel's creation.
    #[serde(default)]
    pub channel_created: bool,
    #[serde(default)]
    pub members: Vec<Entry>,
}

/// An exclusive lock on one journal for a whole import: two imports of the
/// same team would otherwise mint competing identities.
struct JournalLock(PathBuf);

impl JournalLock {
    fn acquire(journal: &Path) -> Result<Self, CliError> {
        let path = journal.with_extension("lock");
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| {
                CliError::Other(format!("could not create {}: {error}", directory.display()))
            })?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Self(path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CliError::Conflict(format!(
                    "another import of this team is running; if none is, remove {}",
                    path.display()
                )))
            }
            Err(error) => Err(CliError::Other(format!(
                "could not lock {}: {error}",
                path.display()
            ))),
        }
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Journal {
    /// The journal file for one team on one relay and owner.
    pub(super) fn path(directory: &Path, team: &str, relay: &str, owner: &str) -> PathBuf {
        let slug: String = team
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let host = relay
            .split("://")
            .nth(1)
            .unwrap_or(relay)
            .split('/')
            .next()
            .unwrap_or_default()
            .replace(':', "-");
        directory.join(format!(
            "{slug}.{host}.{}.json",
            &owner[..owner.len().min(8)]
        ))
    }

    fn load(path: &Path) -> Result<Option<Self>, CliError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|_| {
                CliError::Other(format!("{} is corrupt; fix or remove it", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CliError::Other(format!(
                "could not read {}: {error}",
                path.display()
            ))),
        }
    }

    /// Write atomically, readable only by the owner.
    fn save(&self, path: &Path) -> Result<(), CliError> {
        let failed = |error: std::io::Error| {
            CliError::Other(format!("could not save {}: {error}", path.display()))
        };
        let directory = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory).map_err(failed)?;
        let text = serde_json::to_vec_pretty(self)
            .map_err(|error| CliError::Other(format!("could not encode the journal: {error}")))?;
        let temporary = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temporary).map_err(failed)?;
            file.write_all(&text).map_err(failed)?;
            file.sync_all().map_err(failed)?;
        }
        std::fs::rename(&temporary, path).map_err(failed)
    }
}

pub(super) struct Options {
    /// The team channel's name; the team's name by default.
    pub channel: Option<String>,
    pub model: Option<String>,
    pub journal_dir: PathBuf,
}

/// Import `team` for the owner `client` signs as, and return the report.
/// Agents whose deploy failed are listed under `failed`; see [`outcome`].
pub(super) async fn import(
    client: &BuzzClient,
    manager: &Manager,
    team: &Team,
    options: Options,
) -> Result<Value, CliError> {
    let owner = client.keys().clone();
    let owner_hex = owner.public_key().to_hex();
    let path = Journal::path(
        &options.journal_dir,
        &team.name,
        client.relay_url(),
        &owner_hex,
    );
    let _lock = JournalLock::acquire(&path)?;
    let mut journal = Journal::load(&path)?.unwrap_or_else(|| Journal {
        team: team.name.clone(),
        relay: client.relay_url().to_owned(),
        owner: owner_hex.clone(),
        ..Journal::default()
    });
    if journal.owner != owner_hex
        || journal.relay != client.relay_url()
        || journal.team != team.name
    {
        return Err(CliError::Other(format!(
            "{} belongs to another owner, relay, or team",
            path.display()
        )));
    }

    // 1. Identities, saved before anything is published.
    for member in &team.members {
        if journal
            .members
            .iter()
            .any(|entry| entry.name == member.name)
        {
            continue;
        }
        let keys = Keys::generate();
        let auth_tag = buzz_sdk::nip_oa::compute_auth_tag(&owner, &keys.public_key(), "")
            .map_err(|error| CliError::Other(format!("could not attest the agent: {error}")))?;
        journal.members.push(Entry {
            name: member.name.clone(),
            pubkey: keys.public_key().to_hex(),
            nsec: keys
                .secret_key()
                .to_bech32()
                .map_err(|_| CliError::Other("could not encode the agent key".into()))?,
            auth_tag,
            profile: false,
            listed: false,
            joined: false,
            deployed: false,
            welcome: None,
            request_id: None,
        });
        journal.save(&path)?;
    }
    let identities: Vec<(Member, Keys)> = team
        .members
        .iter()
        .map(|member| {
            let entry = entry(&journal, &member.name)?;
            let keys = Keys::parse(&entry.nsec)
                .map_err(|_| CliError::Other(format!("{} holds an invalid key", path.display())))?;
            Ok((member.clone(), keys))
        })
        .collect::<Result<_, CliError>>()?;
    let roster: Vec<(String, PublicKey)> = identities
        .iter()
        .map(|(member, keys)| (member.name.clone(), keys.public_key()))
        .collect();

    // 2. Profiles and listings.
    for (member, keys) in &identities {
        let auth = entry(&journal, &member.name)?.auth_tag.clone();
        if !entry(&journal, &member.name)?.profile {
            publish_profile(client, member, keys, &auth).await?;
            entry_mut(&mut journal, &member.name)?.profile = true;
            journal.save(&path)?;
        }
        if !entry(&journal, &member.name)?.listed {
            let content =
                json!({"parallelism": 1, "respond_to": "owner-only", "name": member.name});
            let builder =
                EventBuilder::new(Kind::Custom(LISTING_KIND), content.to_string()).tags([
                    tag(&["d", &keys.public_key().to_hex()])?,
                    tag(&["runtime", REMOTE_RUNTIME])?,
                ]);
            submit(client, client.sign_event(builder)?, "agent listing").await?;
            entry_mut(&mut journal, &member.name)?.listed = true;
            journal.save(&path)?;
        }
    }

    // 3. The team channel.
    let channel = match journal.channel_id {
        Some(channel) => channel,
        None => {
            let channel = Uuid::new_v4();
            journal.channel_id = Some(channel);
            journal.save(&path)?;
            channel
        }
    };
    if !journal.channel_created {
        // A lost reply may have created it already: create only when absent.
        if !channel_exists(client, channel).await? {
            let name = options.channel.clone().unwrap_or_else(|| team.name.clone());
            create_channel(client, channel, &name, &team.name).await?;
        }
        journal.channel_created = true;
        journal.save(&path)?;
    }
    for (member, keys) in &identities {
        if entry(&journal, &member.name)?.joined {
            continue;
        }
        let builder = buzz_sdk::build_add_member(channel, &keys.public_key().to_hex(), None)
            .map_err(|error| CliError::Other(format!("build_add_member failed: {error}")))?;
        submit(client, client.sign_event(builder)?, "channel membership").await?;
        entry_mut(&mut journal, &member.name)?.joined = true;
        journal.save(&path)?;
    }

    // 4. One deploy per agent, bound to its welcome thread.
    let (mut failed, mut warnings) = (Vec::new(), Vec::new());
    for (member, keys) in &identities {
        if entry(&journal, &member.name)?.deployed {
            continue;
        }
        if entry(&journal, &member.name)?.welcome.is_none() {
            let welcome = welcome(client, member, keys, channel)?;
            let entry = entry_mut(&mut journal, &member.name)?;
            entry.welcome = Some(serde_json::to_value(&welcome).map_err(|error| {
                CliError::Other(format!("could not record the welcome: {error}"))
            })?);
            entry.request_id = Some(Uuid::new_v4().to_string());
            journal.save(&path)?;
        }
        let saved = entry(&journal, &member.name)?.clone();
        let (Some(welcome), Some(request_id)) = (saved.welcome, saved.request_id) else {
            return Err(CliError::Other(format!(
                "the journal lost {}'s welcome",
                member.name
            )));
        };
        let welcome: nostr::Event = serde_json::from_value(welcome)
            .map_err(|_| CliError::Other(format!("{} holds an invalid welcome", path.display())))?;
        let auth = saved.auth_tag;
        let harness = harness(&member.runtime);
        let prompt = team_prompt(team, &roster, &keys.public_key());
        let launch = launch(
            member,
            harness,
            options.model.as_deref(),
            &prompt,
            &owner.public_key(),
        );
        let target = Deploy {
            member,
            keys,
            auth_tag: &auth,
            channel,
            welcome,
            request_id: &request_id,
        };
        match deploy(client, manager, target, launch).await {
            Ok(paseo_sync) => {
                entry_mut(&mut journal, &member.name)?.deployed = true;
                journal.save(&path)?;
                // The agent runs; only the daemon's agent list is stale.
                if let Some(error) = paseo_sync {
                    warnings.push(json!({"name": member.name, "paseo_sync": error}));
                }
            }
            Err(error) => {
                // A definite refusal started nothing: a retry needs a new
                // request ID, or the manager replays this refusal. An
                // uncertain result keeps the ID so a retry is deduplicated.
                if error.to_string().starts_with(super::REFUSED) {
                    entry_mut(&mut journal, &member.name)?.request_id =
                        Some(Uuid::new_v4().to_string());
                    journal.save(&path)?;
                }
                failed.push(json!({"name": member.name, "error": error.to_string()}));
            }
        }
    }
    let agents: Vec<Value> = identities
        .iter()
        .map(|(member, keys)| {
            let deployed = journal
                .members
                .iter()
                .any(|entry| entry.name == member.name && entry.deployed);
            json!({
                "name": member.name,
                "pubkey": keys.public_key().to_hex(),
                "harness": harness(&member.runtime).id,
                "deployed": deployed,
            })
        })
        .collect();
    Ok(json!({
        "team": team.name,
        "channel_id": channel,
        "agents": agents,
        "failed": failed,
        "warnings": warnings,
        "journal": path.display().to_string(),
    }))
}

/// Fail after reporting when any agent did not deploy, so a rerun is
/// visibly needed.
pub(super) fn outcome(report: &Value) -> Result<(), CliError> {
    let failed = report["failed"].as_array().map_or(0, Vec::len);
    if failed == 0 {
        return Ok(());
    }
    Err(CliError::Other(format!(
        "{failed} agent(s) did not deploy; rerun `buzz paseo import-team` to retry them"
    )))
}

fn entry<'a>(journal: &'a Journal, name: &str) -> Result<&'a Entry, CliError> {
    journal
        .members
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| CliError::Other(format!("the journal has no entry for {name}")))
}

fn entry_mut<'a>(journal: &'a mut Journal, name: &str) -> Result<&'a mut Entry, CliError> {
    journal
        .members
        .iter_mut()
        .find(|entry| entry.name == name)
        .ok_or_else(|| CliError::Other(format!("the journal has no entry for {name}")))
}

fn tag(parts: &[&str]) -> Result<Tag, CliError> {
    Tag::parse(parts.iter().copied())
        .map_err(|error| CliError::Other(format!("invalid tag: {error}")))
}

/// Submit an event that may already be stored: a duplicate is success.
async fn submit_once(client: &BuzzClient, event: nostr::Event, what: &str) -> Result<(), CliError> {
    match submit(client, event, what).await {
        Err(CliError::Conflict(_)) => Ok(()),
        result => result,
    }
}

async fn channel_exists(client: &BuzzClient, channel: Uuid) -> Result<bool, CliError> {
    let response = client
        .query(&json!({"kinds": [39000], "#d": [channel.to_string()], "limit": 1}))
        .await?;
    let events: Vec<Value> = serde_json::from_str(&response)
        .map_err(|_| CliError::Other("invalid channel metadata response".into()))?;
    Ok(!events.is_empty())
}

async fn submit(client: &BuzzClient, event: nostr::Event, what: &str) -> Result<(), CliError> {
    let response = client.submit_event(event).await?;
    super::super::parse_write_response(&response, &format!("{what} changed concurrently; rerun"))
        .map(|_| ())
}

/// The agent's kind 0, signed by the agent and admitted through its
/// attestation, with its avatar uploaded first.
async fn publish_profile(
    client: &BuzzClient,
    member: &Member,
    keys: &Keys,
    auth_tag: &str,
) -> Result<(), CliError> {
    let tag = buzz_sdk::nip_oa::parse_auth_tag(auth_tag)
        .map_err(|_| CliError::Other("invalid agent attestation".into()))?;
    let agent = BuzzClient::new(
        client.relay_url().to_owned(),
        keys.clone(),
        Some(tag),
        Some(auth_tag.to_owned()),
    )?;
    let mut content = json!({"name": member.name, "display_name": member.name, "bot": true});
    if let Some(about) = &member.about {
        content["about"] = json!(about);
    }
    if let Some(avatar) = &member.avatar {
        content["picture"] = json!(agent.upload_bytes(avatar.clone()).await?.url);
    }
    let event = agent.sign_event(EventBuilder::new(Kind::Metadata, content.to_string()))?;
    submit(&agent, event, "agent profile").await
}

async fn create_channel(
    client: &BuzzClient,
    id: Uuid,
    name: &str,
    team: &str,
) -> Result<(), CliError> {
    let about = format!("The {team} agent team.");
    let builder = buzz_sdk::build_create_channel(
        id,
        name,
        Some(buzz_sdk::Visibility::Private),
        Some(buzz_sdk::ChannelKind::Stream),
        Some(&about),
        None,
        false,
    )
    .map_err(|error| CliError::Other(format!("build_create_channel failed: {error}")))?;
    submit(client, client.sign_event(builder)?, "team channel").await
}

/// The owner's signed welcome mention that a member's first deploy answers.
fn welcome(
    client: &BuzzClient,
    member: &Member,
    keys: &Keys,
    channel: Uuid,
) -> Result<nostr::Event, CliError> {
    let agent = keys.public_key().to_hex();
    let content = format!(
        "@{} welcome to the team. Reply with a one-line hello so I know you are running.",
        member.name
    );
    let builder =
        buzz_sdk::build_message(channel, &content, None, &[agent.as_str()], false, &[], &[])
            .map_err(|error| CliError::Other(format!("build_message failed: {error}")))?;
    client.sign_event(builder)
}

/// One member's deploy, bound to its saved welcome and request ID.
struct Deploy<'a> {
    member: &'a Member,
    keys: &'a Keys,
    auth_tag: &'a str,
    channel: Uuid,
    welcome: nostr::Event,
    request_id: &'a str,
}

/// Post the saved welcome (a repeat is a duplicate), then deploy the agent
/// into its thread, signed as the agent. The replay floor lets the new
/// Sandbox read the welcome it answers. Returns the manager's `paseo_sync`
/// warning, if any.
async fn deploy(
    client: &BuzzClient,
    manager: &Manager,
    target: Deploy<'_>,
    mut launch: Value,
) -> Result<Option<Value>, CliError> {
    let Deploy {
        member,
        keys,
        auth_tag,
        channel,
        welcome,
        request_id,
    } = target;
    let agent = keys.public_key().to_hex();
    let (root, floor) = (welcome.id.to_hex(), welcome.created_at.as_secs());
    submit_once(client, welcome, "welcome message").await?;
    launch["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"] = json!(floor.to_string());
    let nsec = keys
        .secret_key()
        .to_bech32()
        .map_err(|_| CliError::Other("could not encode the agent key".into()))?;
    let payload = json!({
        "op": "deploy",
        "request_id": request_id,
        "agent": {
            "name": member.name,
            "relay_url": manager.relay(),
            "private_key_nsec": nsec,
            "auth_tag": auth_tag,
            "env_vars": {},
            "launch": launch,
        },
        "provider_config": {
            "sandbox": {"channel_id": channel.to_string(), "thread_root": root},
            "identity_policy": {"agent_pubkey": agent, "owner_pubkey": client.keys().public_key().to_hex()},
        },
    });
    let response = manager
        .call_as(
            keys,
            auth_tag,
            Op::Deploy,
            request_id,
            payload,
            DEPLOY_BUDGET,
        )
        .await?;
    Ok(response.get("paseo_sync").cloned())
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
