//! Explicit, owner-approved sandbox access through the community relay.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Duration;

use buzz_sdk::sandbox_attach::{self as wire, Authority, Grant, Pending, Scope};
use clap::Subcommand;
use nostr::{Event, EventId, PublicKey, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{client::BuzzClient, error::CliError};

mod snapshot;
#[cfg(unix)]
mod terminal;
#[cfg(test)]
mod tests;

/// Owner approvals, interactive terminals, and read-only sandbox snapshots.
#[derive(Subcommand)]
pub enum SandboxCmd {
    /// Validate a local captured bundle before any Git import (internal worker seam)
    #[command(hide = true)]
    InspectBundle {
        /// Captured bundle file
        file: std::path::PathBuf,
    },
    /// Transfer a read-only snapshot or return a patch through the thread
    Snapshot {
        #[command(subcommand)]
        command: snapshot::SnapshotCmd,
    },
    /// Attach a local TTY; omitted targets are selected from BUZZ_PASEO_AGENTS
    Attach {
        /// Channel UUID containing the sandbox thread
        #[arg(long, requires = "thread")]
        channel: Option<Uuid>,
        /// Thread root or a message inside the thread
        #[arg(long, requires = "channel")]
        thread: Option<EventId>,
        /// Agent public key (hex or npub); otherwise infer from the target
        #[arg(long)]
        agent: Option<String>,
        /// Human owner; hosted gateways must set this explicitly
        #[arg(long, env = "BUZZ_PASEO_OWNER")]
        owner: Option<String>,
        /// Session lifetime in minutes, maximum 60
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        minutes: u64,
    },
    /// Approve one pending request with the sandbox owner's primary key
    Authorize {
        /// Pending terminal request event ID
        #[arg(long)]
        request: EventId,
        /// Approval lifetime in minutes, capped by the pending request
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        minutes: u64,
    },
    /// Revoke one owner-signed approval by NIP-09 deletion
    Revoke {
        /// Owner approval event ID
        #[arg(long)]
        grant: EventId,
    },
}

/// Print bounded local preflight metadata, without reading credentials or connecting.
pub(crate) fn inspect_bundle_cmd(file: &std::path::Path) -> Result<(), CliError> {
    let summary = crate::sandbox_archive::inspect_bundle_file(file).map_err(failure)?;
    let encoded = serde_json::to_string(&summary).map_err(failure)?;
    if encoded.len() > 16 * 1024 * 1024 {
        return Err(failure("bundle metadata exceeded budget"));
    }
    println!("{encoded}");
    Ok(())
}

fn failure(message: impl ToString) -> CliError {
    CliError::Other(message.to_string())
}
fn now() -> u64 {
    Timestamp::now().as_secs()
}
fn key(value: &str) -> Result<PublicKey, CliError> {
    PublicKey::parse(value).map_err(|_| CliError::Usage("invalid public key".into()))
}
fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|k| k == name));
    let value = tags.next()?.as_slice().get(1)?.as_str();
    tags.next().is_none().then_some(value)
}

fn websocket_url(relay: &str) -> Result<String, CliError> {
    let mut url = url::Url::parse(relay).map_err(failure)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CliError::Usage(
            "sandbox access requires an HTTPS/WSS relay without URL credentials, query or fragment"
                .into(),
        ));
    }
    url.set_scheme("wss")
        .map_err(|_| failure("invalid relay URL"))?;
    Ok(url.to_string())
}

async fn query(client: &BuzzClient, mut filter: Value) -> Result<Vec<Event>, CliError> {
    filter["consistency"] = json!("strong");
    let result = timeout(Duration::from_secs(5), client.query(&filter))
        .await
        .map_err(|_| failure("sandbox authority query timed out"))??;
    if result.len() > 2 * 1024 * 1024 {
        return Err(failure("sandbox query exceeded response budget"));
    }
    let events: Vec<Event> = serde_json::from_str(&result).map_err(failure)?;
    let limit = filter["limit"].as_u64().unwrap_or(1) as usize;
    if events.len() > limit || events.iter().any(|event| event.verify().is_err()) {
        return Err(failure("invalid sandbox query response"));
    }
    Ok(events)
}
async fn one(client: &BuzzClient, filter: Value) -> Result<Event, CliError> {
    let mut events = query(client, filter).await?;
    if events.len() != 1 {
        return Err(failure("sandbox authority is absent or ambiguous"));
    }
    Ok(events.remove(0))
}
async fn publish(client: &BuzzClient, event: Event) -> Result<(), CliError> {
    let response = timeout(Duration::from_secs(5), client.submit_event(event))
        .await
        .map_err(|_| failure("sandbox publish timed out; acceptance is unknown"))??;
    super::parse_write_response(&response, "sandbox metadata changed concurrently")?;
    Ok(())
}
async fn relay_key(client: &BuzzClient) -> Result<PublicKey, CliError> {
    let raw = timeout(Duration::from_secs(5), client.get_public("/info"))
        .await
        .map_err(|_| failure("relay metadata timed out"))??;
    let info: Value = serde_json::from_str(&raw).map_err(failure)?;
    key(info["pubkey"]
        .as_str()
        .ok_or_else(|| failure("relay metadata has no signing key"))?)
}
async fn current_pending(client: &BuzzClient, pending: &Pending) -> Result<(), CliError> {
    let head = one(client, json!({"kinds":[30624],"authors":[pending.requester()],"#d":[format!("terminal:{}:request",pending.session())],"limit":1})).await?;
    if head.id != pending.id() || Pending::parse(&head, now()).is_err() {
        return Err(failure("terminal request was replaced, revoked or expired"));
    }
    Ok(())
}
async fn authorized(
    client: &BuzzClient,
    owner: PublicKey,
    relay: PublicKey,
    pending: &Pending,
    grant: &Grant,
) -> Result<(), CliError> {
    timeout(Duration::from_secs(5), async {
        let (head, roster, _, delegations, profiles) = tokio::try_join!(
            one(client,json!({"kinds":[30624],"authors":[owner],"#d":[format!("terminal:{}:grant",grant.session())],"limit":1})),
            one(client,json!({"kinds":[39002],"authors":[relay],"#d":[grant.scope().channel],"limit":1})),
            current_pending(client,pending),
            query(client,json!({"kinds":[30181],"authors":[owner],"limit":32})),
            query(client,json!({"kinds":[0],"authors":[grant.requester()],"limit":1})),
        )?;
        if head.id != grant.id() || !grant.authorized(pending,Authority{owner,relay,roster:&roster,delegations:&delegations,profiles:&profiles},now()) {
            return Err(CliError::Auth("terminal approval, membership or gateway authority ended".into()));
        }
        Ok(())
    }).await.map_err(|_| failure("terminal authorization refresh timed out"))?
}

/// Execute sandbox commands. Hosted gateways cannot sign owner approvals.
pub async fn dispatch(command: SandboxCmd, client: &BuzzClient) -> Result<(), CliError> {
    let ws_url = websocket_url(client.relay_url())?;
    if client.auth_tag_owner_hex().is_some() {
        return Err(CliError::Auth(
            "agent attestations cannot authorize sandbox attachment".into(),
        ));
    }
    match command {
        SandboxCmd::InspectBundle { file } => inspect_bundle_cmd(&file),
        SandboxCmd::Snapshot { command } => snapshot::dispatch(command, client).await,
        SandboxCmd::Authorize { request, minutes } => {
            let event = one(client, json!({"ids":[request],"kinds":[30624],"limit":1})).await?;
            if event.id != request {
                return Err(failure("relay returned another request"));
            }
            let pending = Pending::parse(&event, now()).map_err(failure)?;
            let owner = client.keys().public_key();
            let expiry = pending.expires_at().min(now().saturating_add(minutes * 60));
            let approval = wire::approve(client.keys(), &pending, expiry).map_err(failure)?;
            let grant = Grant::parse(&approval, pending.scope(), now()).map_err(failure)?;
            let relay = relay_key(client).await?;
            // Check current authority before publication, using the locally signed approval.
            let (roster, delegations, profiles, _) = tokio::try_join!(
                one(
                    client,
                    json!({"kinds":[39002],"authors":[relay],"#d":[pending.scope().channel],"limit":1})
                ),
                query(
                    client,
                    json!({"kinds":[30181],"authors":[owner],"limit":32})
                ),
                query(
                    client,
                    json!({"kinds":[0],"authors":[pending.requester()],"limit":1})
                ),
                current_pending(client, &pending),
            )?;
            if !grant.authorized(
                &pending,
                Authority {
                    owner,
                    relay,
                    roster: &roster,
                    delegations: &delegations,
                    profiles: &profiles,
                },
                now(),
            ) {
                return Err(CliError::Auth(
                    "requester is not an admitted owner/live gateway".into(),
                ));
            }
            publish(client, approval.clone()).await?;
            println!(
                "{}",
                json!({"event_id":approval.id,"accepted":true,"expires_at":expiry})
            );
            Ok(())
        }
        SandboxCmd::Revoke { grant } => {
            let event = one(client, json!({"ids":[grant],"kinds":[30624],"limit":1})).await?;
            if event.id != grant
                || event.pubkey != client.keys().public_key()
                || !tag(&event, "d").is_some_and(|d| d.ends_with(":grant"))
            {
                return Err(CliError::Auth(
                    "only this approval's owner can revoke it".into(),
                ));
            }
            let channel = tag(&event, "h").ok_or_else(|| failure("approval has no channel"))?;
            let builder =
                nostr::EventBuilder::new(nostr::Kind::EventDeletion, "sandbox approval revoked")
                    .tags([
                        nostr::Tag::parse(["e", &grant.to_hex()]).map_err(failure)?,
                        nostr::Tag::parse(["h", channel]).map_err(failure)?,
                    ]);
            let deletion = client.sign_event(builder)?;
            publish(client, deletion.clone()).await?;
            println!("{}", json!({"event_id":deletion.id,"accepted":true}));
            Ok(())
        }
        SandboxCmd::Attach {
            channel,
            thread,
            agent,
            owner,
            minutes,
        } => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                return Err(CliError::Usage(
                    "sandbox attach requires a local TTY".into(),
                ));
            }
            let owner = owner
                .as_deref()
                .map(key)
                .transpose()?
                .unwrap_or(client.keys().public_key());
            let scope = select_target(client, channel, thread, agent.as_deref()).await?;
            let relay = relay_key(client).await?;
            let event = wire::request(client.keys(), scope, Uuid::new_v4(), now() + minutes * 60)
                .map_err(failure)?;
            let pending = Pending::parse(&event, now()).map_err(failure)?;
            publish(client, event).await?;
            if owner == client.keys().public_key() {
                publish(
                    client,
                    wire::approve(client.keys(), &pending, pending.expires_at())
                        .map_err(failure)?,
                )
                .await?;
            } else {
                eprintln!("Owner approval required for channel {} thread {} agent {} requester {}.\nOn the owner's machine, using the same relay, run:\nbuzz sandbox authorize --request {}",scope.channel,scope.thread,scope.agent,client.keys().public_key(),pending.id());
            }
            let grant = loop {
                if now() >= pending.expires_at() {
                    return Err(failure(
                        "terminal request expired while awaiting owner approval",
                    ));
                }
                current_pending(client, &pending).await?;
                let approvals = query(client,json!({"kinds":[30624],"authors":[owner],"#d":[format!("terminal:{}:grant",pending.session())],"limit":1})).await?;
                if let Some(event) = approvals.first() {
                    let grant = Grant::parse(event, scope, now()).map_err(failure)?;
                    authorized(client, owner, relay, &pending, &grant).await?;
                    break grant;
                }
                sleep(Duration::from_secs(2)).await;
            };
            #[cfg(unix)]
            return terminal::attach(client, &ws_url, owner, relay, &pending, &grant).await;
            #[cfg(not(unix))]
            {
                let _ = (ws_url, owner, relay, pending, grant);
                Err(CliError::Usage(
                    "sandbox terminals currently require Unix".into(),
                ))
            }
        }
    }
}

#[derive(Deserialize)]
struct Mapping {
    label: String,
    agent: String,
    channel: Uuid,
}
fn mappings(value: &str) -> Result<Vec<Mapping>, CliError> {
    if value.len() > 65536 {
        return Err(CliError::Usage("BUZZ_PASEO_AGENTS exceeds 64 KiB".into()));
    }
    let maps: Vec<Mapping> = serde_json::from_str(value).map_err(|_| {
        CliError::Usage("BUZZ_PASEO_AGENTS must contain agent/channel mappings".into())
    })?;
    if maps.len() > 64 {
        return Err(CliError::Usage(
            "at most 64 configured agents are supported".into(),
        ));
    }
    for map in &maps {
        key(&map.agent)?;
    }
    Ok(maps)
}
fn label(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
        })
        .take(100)
        .collect()
}
fn choose(labels: &[String]) -> Result<usize, CliError> {
    if labels.is_empty() {
        return Err(CliError::Usage(
            "no targets found; start an agent thread first".into(),
        ));
    }
    for (index, title) in labels.iter().enumerate() {
        eprintln!("{}. {}", index + 1, label(title));
    }
    eprint!("Choose 1-{}: ", labels.len());
    std::io::stderr().flush().map_err(failure)?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .take(32)
        .read_line(&mut answer)
        .map_err(failure)?;
    answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0 && *n <= labels.len())
        .map(|n| n - 1)
        .ok_or_else(|| CliError::Usage("invalid target selection".into()))
}
async fn select_target(
    client: &BuzzClient,
    channel: Option<Uuid>,
    thread: Option<EventId>,
    agent: Option<&str>,
) -> Result<Scope, CliError> {
    let maps = mappings(&std::env::var("BUZZ_PASEO_AGENTS").unwrap_or_else(|_| "[]".into()))?;
    let (channel, thread, agent) = match (channel, thread) {
        (Some(channel), Some(thread)) => (channel, thread, agent.map(key).transpose()?),
        (None, None) => {
            let index = choose(
                &maps
                    .iter()
                    .map(|m| format!("{} ({})", m.label, m.channel))
                    .collect::<Vec<_>>(),
            )?;
            let map = &maps[index];
            let agent = key(&map.agent)?;
            let roots = query(
                client,
                json!({"kinds":[9],"#h":[map.channel],"#p":[agent],"limit":64}),
            )
            .await?;
            let roots: Vec<_> = roots
                .into_iter()
                .filter(|event| {
                    tag(event, "h") == Some(map.channel.to_string().as_str())
                        && buzz_core::nip10::parse_thread_markers(&event.tags)
                            .resolve()
                            .is_none()
                })
                .collect();
            let selected = choose(
                &roots
                    .iter()
                    .map(|event| format!("{} {}", event.id, label(&event.content)))
                    .collect::<Vec<_>>(),
            )?;
            (map.channel, roots[selected].id, Some(agent))
        }
        _ => {
            return Err(CliError::Usage(
                "provide both --channel and --thread".into(),
            ))
        }
    };
    let event = one(
        client,
        json!({"kinds":[9],"ids":[thread],"#h":[channel],"limit":1}),
    )
    .await?;
    if event.id != thread || tag(&event, "h") != Some(channel.to_string().as_str()) {
        return Err(failure("thread does not belong to channel"));
    }
    let root = buzz_core::nip10::parse_thread_markers(&event.tags)
        .resolve()
        .map(|(root, _)| EventId::from_hex(&root).map_err(failure))
        .transpose()?
        .unwrap_or(event.id);
    let root_event = if root == event.id {
        event
    } else {
        one(
            client,
            json!({"kinds":[9],"ids":[root],"#h":[channel],"limit":1}),
        )
        .await?
    };
    if root_event.id != root
        || tag(&root_event, "h") != Some(channel.to_string().as_str())
        || buzz_core::nip10::parse_thread_markers(&root_event.tags)
            .resolve()
            .is_some()
    {
        return Err(failure("invalid thread root"));
    }
    let mut configured: Vec<_> = maps
        .iter()
        .filter(|map| map.channel == channel)
        .filter_map(|map| key(&map.agent).ok())
        .filter(|key| {
            root_event.tags.iter().any(|tag| {
                tag.as_slice().first().is_some_and(|name| name == "p")
                    && tag.as_slice().get(1) == Some(&key.to_hex())
            })
        })
        .collect();
    configured.sort();
    configured.dedup();
    let inferred = if configured.len() == 1 {
        configured.first().copied()
    } else if configured.is_empty() {
        tag(&root_event, "p").and_then(|p| key(p).ok())
    } else {
        None
    };
    let agent = agent
        .or(inferred)
        .ok_or_else(|| CliError::Usage("ambiguous agent; provide --agent".into()))?;
    Ok(Scope {
        channel,
        thread: root,
        agent,
    })
}
