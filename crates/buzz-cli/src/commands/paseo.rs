//! Owner-only access to the existing encrypted manager profile operation.
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
        #[arg(long)]
        manager: String,
    },
}

pub(crate) async fn dispatch(command: PaseoCmd, client: &BuzzClient) -> Result<(), CliError> {
    let PaseoCmd::Pairing { manager } = command;
    if client.auth_tag_owner_hex().is_some() {
        return Err(CliError::Auth(
            "pairing requires the owner's direct signing key".into(),
        ));
    }
    if !std::io::stderr().is_terminal() {
        return Err(CliError::Usage("pairing requires interactive stderr; its password is never written to JSON or redirected output".into()));
    }
    let manager = PublicKey::from_hex(&manager)
        .map_err(|_| CliError::Usage("invalid manager public key".into()))?;
    let relay = client.relay_url().replacen("https://", "wss://", 1);
    if !relay.starts_with("wss://") {
        return Err(CliError::Usage("pairing requires a WSS relay".into()));
    }
    let (public, password) =
        exchange(&relay, client.keys(), manager, Duration::from_secs(15)).await?;
    eprintln!(
        "Paseo pairing password (private; interactive stderr only): {}",
        password.as_str()
    );
    println!("{public}");
    Ok(())
}

fn unavailable() -> CliError {
    CliError::Other(
        "Paseo pairing unavailable; check manager, enrollment and relay, then retry".into(),
    )
}

async fn exchange(
    relay: &str,
    owner: &Keys,
    manager: PublicKey,
    budget: Duration,
) -> Result<(Value, Zeroizing<String>), CliError> {
    tokio::time::timeout(budget, async {
        let request = Request::new(Op::Profile, uuid::Uuid::new_v4().to_string(), json!({}));
        let event = build_request(owner, &manager, None, &request, Timestamp::now()).map_err(|_| unavailable())?;
        let mut ws = NostrWsConnection::connect_authenticated(relay, owner, None).await.map_err(|_| unavailable())?;
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
                    if opened.status == Status::Done { done = Some(pairing(opened.response.as_ref().ok_or_else(unavailable)?)?); }
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(unavailable()),
                _ => {}
            }
            if accepted && done.is_some() { return done.ok_or_else(unavailable); }
        }
        Err(unavailable())
    }).await.map_err(|_| unavailable())?
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

#[cfg(test)]
mod tests;
