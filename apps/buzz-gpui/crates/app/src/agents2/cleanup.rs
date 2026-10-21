//! Finishing an Agents2 Delete in its community (buzz-app's
//! `Agents2Service.cleanup` with `removeAgentFromChannels` and the NIP-IA
//! identity archive): the owner removes the agent from every channel, then
//! archives its identity so it drops out of member lists and mention
//! suggestions. The owner signs both, so neither needs the deleted key.
//! Nothing is final until each is confirmed by a fresh relay read.

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::members::{Change, MemberChange};
use buzz_relay_client::socket::Filter;
use buzz_relay_client::{RelayHttp, Session, Verifier};
use nostr::prelude::Event;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Rosters past this many cannot be confirmed in one read.
const ROSTER_LIMIT: usize = 500;
/// The archive snapshot is at most this large.
const ARCHIVE_LIMIT: usize = 2 * 1024 * 1024;

fn filter(value: Value) -> Filter {
    value.as_object().cloned().unwrap_or_default()
}

fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == name)
        .and_then(|tag| tag.as_slice().get(1))
        .map(String::as_str)
}

fn names(event: &Event, name: &str, value: &str) -> bool {
    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.first().map(String::as_str) == Some(name)
            && slice.get(1).map(String::as_str) == Some(value)
    })
}

/// Removes `agent` from its channels and archives it, as `owner`.
pub(super) async fn clean(
    session: &Session,
    owner: &Arc<Identity>,
    agent: &str,
) -> Result<(), String> {
    let authority = session
        .snapshot()
        .borrow()
        .relay
        .as_ref()
        .map(|relay| relay.authority.clone())
        .filter(|authority| !authority.is_empty());
    let http = RelayHttp::new(session.origin().clone(), owner.clone());
    leave_channels(session, &http, authority.as_deref(), agent).await?;
    // Without the relay's key there is no archive to write to.
    if let Some(authority) = authority {
        archive(&http, owner, &authority, agent).await?;
    }
    super::listings::delete(&http, owner, agent).await
}

/// What follows a hosted agent's removal by its provider
/// (`removeRelayAgent`): it is archived where there is consent, its
/// record deleted, then its channels left, as far as they will go.
pub(super) async fn clean_hosted(
    session: &Session,
    owner: &Arc<Identity>,
    agent: &str,
) -> Result<(), String> {
    let authority = session
        .snapshot()
        .borrow()
        .relay
        .as_ref()
        .map(|relay| relay.authority.clone())
        .filter(|authority| !authority.is_empty());
    let http = RelayHttp::new(session.origin().clone(), owner.clone());
    if let Some(authority) = &authority {
        archive(&http, owner, authority, agent).await?;
    }
    super::listings::delete(&http, owner, agent).await?;
    if let Err(error) = leave_channels(session, &http, authority.as_deref(), agent).await {
        tracing::warn!(%error, "removed hosted agent did not leave every channel");
    }
    Ok(())
}

/// The relay's rosters naming the agent, plus the viewer's loaded channels
/// that list it; each is left through a confirmed owner removal.
async fn leave_channels(
    session: &Session,
    http: &RelayHttp,
    authority: Option<&str>,
    agent: &str,
) -> Result<(), String> {
    let mut pending: BTreeSet<String> = session
        .snapshot()
        .borrow()
        .channels
        .iter()
        .filter(|channel| channel.members.iter().any(|member| member == agent))
        .map(|channel| channel.id.clone())
        .collect();
    let authority = authority.ok_or("Channel membership is unavailable.")?;
    let rosters = http
        .query(&[filter(json!({
            "kinds": [39002],
            "authors": [authority],
            "#p": [agent],
            "limit": ROSTER_LIMIT,
        }))])
        .await
        .map_err(|error| error.to_string())?;
    if rosters.len() >= ROSTER_LIMIT {
        return Err("Too many channels to confirm. Retry later.".into());
    }
    for roster in &rosters {
        if roster.kind.as_u16() == 39002
            && roster.pubkey.to_hex() == authority
            && names(roster, "p", agent)
            && let Some(id) = tag(roster, "d")
        {
            pending.insert(id.to_owned());
        }
    }
    let mut failures = 0;
    for channel in &pending {
        let state = session.channel_state(channel).await?;
        let Some(role) = state.role(agent) else {
            continue;
        };
        let change = MemberChange {
            pubkey: agent.to_owned(),
            expected: role,
            change: Change::Remove,
        };
        if let Err(error) = session.change_member(channel, change).await {
            tracing::warn!(%channel, %error, "deleted agent was not removed from a channel");
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(format!(
            "The relay did not confirm removal from {failures} channel{}. Retry.",
            if failures == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Archived {
    Yes,
    No,
}

/// The relay's NIP-IA snapshot (kind 13535) says whether `agent` is
/// archived. An unfinished or invalid read says nothing.
async fn archived(http: &RelayHttp, authority: &str, agent: &str) -> Result<Archived, String> {
    let unknown = || "Archive state is unknown".to_string();
    let events = http
        .query(&[filter(
            json!({ "kinds": [13535], "authors": [authority], "limit": 1 }),
        )])
        .await
        .map_err(|_| unknown())?;
    // A relay that never archived anyone has no snapshot yet: nobody is
    // archived (the buzz CLI's reading; buzz-app waits for a snapshot).
    let event = match events.as_slice() {
        [] => return Ok(Archived::No),
        [event] => event,
        _ => return Err(unknown()),
    };
    let markers: Vec<_> = event.tags.iter().filter(|tag| tag.kind() == "-").collect();
    if event.kind.as_u16() != 13535
        || event.pubkey.to_hex() != authority
        || !event.content.is_empty()
        || markers.len() != 1
        || markers[0].as_slice().len() != 1
        || serde_json::to_vec(event).map_or(usize::MAX, |bytes| bytes.len()) > ARCHIVE_LIMIT
    {
        return Err(unknown());
    }
    Ok(if names(event, "p", agent) {
        Archived::Yes
    } else {
        Archived::No
    })
}

/// How the owner may archive an identity (`archives.consent`): its owner
/// attestation, a relay owner or admin role, or both; neither for the
/// viewer's own. `None` means no path.
#[derive(Default)]
struct Consent {
    auth: Option<Vec<String>>,
    admin: bool,
}

async fn consent(
    http: &RelayHttp,
    viewer: &str,
    authority: &str,
    agent: &str,
) -> Result<Option<Consent>, String> {
    // NIP-IA archival of yourself needs no evidence.
    if agent == viewer {
        return Ok(Some(Consent::default()));
    }
    let events = http
        .query(&[
            filter(json!({ "kinds": [0], "authors": [agent], "limit": 1 })),
            filter(json!({ "kinds": [13534], "authors": [authority], "limit": 1 })),
        ])
        .await
        .map_err(|error| error.to_string())?;
    let verifier = Verifier::new(4);
    let newest = |kind: u16, author: &str| {
        events
            .iter()
            .filter(|event| event.kind.as_u16() == kind && event.pubkey.to_hex() == author)
            .max_by_key(|event| (event.created_at, std::cmp::Reverse(event.id)))
            .cloned()
    };
    let profile = newest(0, agent).and_then(|event| verifier.verify(event).ok());
    // NIP-IA ownership ignores kind clauses; time bounds apply when the
    // request is signed (`authTagOwner`).
    let auth = profile.as_ref().and_then(|profile| {
        let mut tags = profile.tags_named("auth");
        let tag = tags.next()?.to_vec();
        (tags.next().is_none()
            && buzz_relay_client::profiles::auth_tag_owner(agent, &tag).as_deref() == Some(viewer))
        .then_some(tag)
    });
    let role = newest(13534, authority).and_then(|roster| {
        roster
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .find(|tag| {
                tag.first().map(String::as_str) == Some("member")
                    && tag.get(1).map(String::as_str) == Some(viewer)
            })
            .and_then(|tag| tag.get(2).cloned())
    });
    let admin = matches!(role.as_deref(), Some("owner" | "admin"));
    Ok((auth.is_some() || admin).then_some(Consent { auth, admin }))
}

/// Archives `agent` unless it already is, when a consent path exists.
async fn archive(
    http: &RelayHttp,
    owner: &Arc<Identity>,
    authority: &str,
    agent: &str,
) -> Result<(), String> {
    // Without an owner-attested profile the agent is not in the directory,
    // so there is nothing to hide.
    match request(http, owner, authority, agent, true).await {
        Err(Refusal::NoConsent) => Ok(()),
        Err(Refusal::Failed(error)) => Err(error),
        Ok(()) => Ok(()),
    }
}

enum Refusal {
    /// Neither the target's owner attestation nor a relay role allows it.
    NoConsent,
    Failed(String),
}

/// Archives (9035) or unarchives (9036) `target` as `owner`, with its owner
/// attestation or the owner's relay role as consent, and answers once the
/// relay's archive shows it (`archives.request`).
async fn request(
    http: &RelayHttp,
    owner: &Arc<Identity>,
    authority: &str,
    target: &str,
    archive: bool,
) -> Result<(), Refusal> {
    // Permission first, as buzz-app asks it before anything else.
    let path = consent(http, &owner.pubkey_hex(), authority, target)
        .await
        .map_err(Refusal::Failed)?
        .ok_or(Refusal::NoConsent)?;
    // Role authority stands alone, so no credential whose time bounds
    // could fail it is attached; one that is attached must hold for the
    // request itself, whatever the archive shows now.
    let auth = path.auth.filter(|_| !path.admin);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(auth) = &auth
        && !buzz_relay_client::profiles::within_time_bounds(
            auth.get(2).map_or("", String::as_str),
            now,
        )
    {
        return Err(Refusal::NoConsent);
    }
    let wanted = if archive { Archived::Yes } else { Archived::No };
    if archived(http, authority, target)
        .await
        .map_err(Refusal::Failed)?
        == wanted
    {
        return Ok(());
    }
    let mut tags = vec![
        vec!["-".to_owned()],
        vec!["p".to_owned(), target.to_owned()],
    ];
    tags.extend(auth);
    let kind = if archive { 9035 } else { 9036 };
    let failed = |error: String| Refusal::Failed(error);
    let unsigned = owner
        .template(Purpose::Content, kind, now, tags, String::new())
        .map_err(|error| failed(error.to_string()))?;
    let signed = owner
        .sign(Purpose::Content, unsigned)
        .map_err(|error| failed(error.to_string()))?;
    let body = serde_json::to_string(&signed).map_err(|error| failed(error.to_string()))?;
    let uncertain = match http.post("/events", body).await {
        Ok(receipt) => {
            let receipt: Value = serde_json::from_slice(&receipt).unwrap_or(Value::Null);
            if receipt.get("accepted").and_then(Value::as_bool) == Some(false) {
                let reason = receipt
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("refused");
                return Err(failed(format!(
                    "The relay refused the archive request: {reason}"
                )));
            }
            None
        }
        Err(error) => Some(error.to_string()),
    };
    if archived(http, authority, target)
        .await
        .map_err(Refusal::Failed)?
        == wanted
    {
        Ok(())
    } else {
        Err(failed(uncertain.unwrap_or_else(|| {
            "The relay did not confirm the change. Retry.".into()
        })))
    }
}

/// Archives or unarchives `target` in `session`'s community as `owner`,
/// which needs its owner attestation or a relay owner or admin role.
/// How `owner` may archive or unarchive `target` in the community
/// (`archives.consent`): `{"auth"}` with its owner attestation, `{"admin":
/// true}` by a relay role, or `null` with no path.
pub(crate) async fn archive_consent(
    session: &Session,
    owner: &Arc<Identity>,
    target: &str,
) -> Result<serde_json::Value, String> {
    let authority = session
        .snapshot()
        .borrow()
        .relay
        .as_ref()
        .map(|relay| relay.authority.clone())
        .filter(|authority| !authority.is_empty());
    // Without the relay's authority there is no path (`null`).
    let Some(authority) = authority else {
        return Ok(serde_json::Value::Null);
    };
    let http = RelayHttp::new(session.origin().clone(), owner.clone());
    Ok(
        match consent(&http, &owner.pubkey_hex(), &authority, target).await? {
            Some(Consent { auth, admin }) => {
                let mut consent = json!({});
                if let Some(auth) = auth {
                    consent["auth"] = json!(auth);
                }
                if admin {
                    consent["admin"] = json!(true);
                }
                consent
            }
            None => serde_json::Value::Null,
        },
    )
}

pub(crate) async fn set_archived(
    session: &Session,
    owner: &Arc<Identity>,
    target: &str,
    archive: bool,
) -> Result<(), String> {
    let authority = session
        .snapshot()
        .borrow()
        .relay
        .as_ref()
        .map(|relay| relay.authority.clone())
        .filter(|authority| !authority.is_empty())
        .ok_or("This community has no archive.")?;
    let http = RelayHttp::new(session.origin().clone(), owner.clone());
    request(&http, owner, &authority, target, archive)
        .await
        .map_err(|refusal| match refusal {
            Refusal::NoConsent => "You can no longer archive this identity".to_owned(),
            Refusal::Failed(error) => error,
        })
}
