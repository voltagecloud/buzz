//! Agent-authored first acceptance of an owner-approved attachment.

use buzz_core::tenant::TenantContext;
use buzz_sdk::sandbox_attach::Scope;
use nostr::{Event, PublicKey};
use serde::Deserialize;
use uuid::Uuid;

use super::ingest::IngestError;
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
enum Audit {
    Terminal {
        version: u8,
        session: Uuid,
        status: String,
        grant: Event,
        bytes_in: u64,
        bytes_out: u64,
    },
    Snapshot {
        version: u8,
        session: Uuid,
        status: String,
        grant: Event,
        bytes: u64,
        reason: Option<serde_json::Value>,
    },
}

fn one<'a>(event: &'a Event, name: &str) -> Result<&'a str, String> {
    let mut tags = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().is_some_and(|key| key == name));
    let Some([_, value]) = tags.next() else {
        return Err("missing or malformed acceptance tag".into());
    };
    if tags.next().is_some() {
        return Err("ambiguous acceptance tag".into());
    }
    Ok(value)
}

fn metadata(event: &Event) -> Result<Option<PublicKey>, String> {
    if u32::from(event.kind.as_u16()) != buzz_core::kind::KIND_SANDBOX_ATTACH {
        return Ok(None);
    }
    let candidate = event.tags.iter().any(|tag| {
        tag.as_slice().first().is_some_and(|key| key == "d")
            && tag
                .as_slice()
                .get(1)
                .is_some_and(|value| value.ends_with(":accepted"))
    }) || serde_json::from_str::<serde_json::Value>(&event.content)
        .ok()
        .is_some_and(|value| value["status"] == "accepted");
    if !candidate {
        return Ok(None);
    }
    let invalid = || "invalid attachment acceptance metadata".to_owned();
    let (mode, session) =
        buzz_core::sandbox_attach::acceptance_coordinate(event).ok_or_else(invalid)?;
    if event.content.len() > 8192
        || event.tags.iter().any(|tag| {
            tag.as_slice()
                .first()
                .is_some_and(|key| key == "expiration")
        })
    {
        return Err("attachment acceptance must be bounded and have no expiration".into());
    }
    let audit: Audit = serde_json::from_str(&event.content).map_err(|_| invalid())?;
    let (version, body_mode, body_session, status, grant, empty) = match audit {
        Audit::Terminal {
            version,
            session,
            status,
            grant,
            bytes_in,
            bytes_out,
        } => (
            version,
            "terminal",
            session,
            status,
            grant,
            bytes_in == 0 && bytes_out == 0,
        ),
        Audit::Snapshot {
            version,
            session,
            status,
            grant,
            bytes,
            reason,
        } => (
            version,
            "snapshot",
            session,
            status,
            grant,
            bytes == 0 && reason.is_none(),
        ),
    };
    if version != 1
        || body_mode != mode
        || body_session != session
        || status != "accepted"
        || !empty
    {
        return Err(invalid());
    }
    let nonce = one(event, "buzz:attach-claim")?;
    if Uuid::parse_str(nonce).map_err(|_| invalid())?.to_string() != nonce {
        return Err(invalid());
    }
    let body: serde_json::Value = serde_json::from_str(&grant.content).map_err(|_| invalid())?;
    let scope: Scope = serde_json::from_value(body["scope"].clone()).map_err(|_| invalid())?;
    let (approved_session, requester) = if mode == "terminal" {
        let approved =
            buzz_sdk::sandbox_attach::Grant::parse(&grant, scope, event.created_at.as_secs())
                .map_err(|_| invalid())?;
        (approved.session(), approved.requester())
    } else {
        let approved = buzz_sdk::sandbox_attach::snapshot::Grant::parse(
            &grant,
            scope,
            event.created_at.as_secs(),
        )
        .map_err(|_| invalid())?;
        (approved.session(), approved.requester())
    };
    if approved_session != session
        || event.pubkey != scope.agent
        || grant.pubkey == scope.agent
        || one(&grant, "h")? != scope.channel.to_string()
        || one(&grant, "e")? != scope.thread.to_hex()
        || one(&grant, "p")? != scope.agent.to_hex()
        || one(event, "h")? != scope.channel.to_string()
        || one(event, "e")? != scope.thread.to_hex()
        || one(event, "p")? != requester.to_hex()
    {
        return Err(invalid());
    }
    Ok(Some(grant.pubkey))
}

/// Ordinary requests/grants remain mutable and revocable. An acceptance is
/// signed only by the recorded agent and retains the owner's signed approval.
pub(super) async fn validate(
    tenant: &TenantContext,
    event: &Event,
    state: &AppState,
) -> Result<(), IngestError> {
    let Some(owner) =
        metadata(event).map_err(|error| IngestError::Rejected(format!("invalid: {error}")))?
    else {
        return Ok(());
    };
    let owned = state
        .db
        .is_agent_owner(
            tenant.community(),
            event.pubkey.as_bytes(),
            owner.as_bytes(),
        )
        .await
        .map_err(|error| {
            IngestError::Internal(format!("attachment owner lookup failed: {}", error.kind()))
        })?;
    if !owned {
        return Err(IngestError::Rejected(
            "restricted: only the approved agent may claim attachment acceptance".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod postgres_tests;
