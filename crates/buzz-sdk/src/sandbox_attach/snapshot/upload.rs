//! Upload recovery metadata survives sandbox loss without releasing a blob key.
//! This is an audit record, never proof of current access or physical deletion.

use super::*;

/// What the producer knows about one exact ciphertext upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Confirmed on the relay before any PUT starts; the blob may not exist.
    Planned,
    /// The server confirmed the exact hash and size; bytes remain in media storage.
    Uploaded,
    /// The PUT was interrupted or ambiguous; the blob may exist.
    Unconfirmed,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u8,
    mode: String,
    session: Uuid,
    state: State,
    grant: Event,
    claim: EventId,
    sha256: String,
    size: u64,
    expires_at: u64,
}

fn coordinate(session: Uuid, state: State) -> String {
    let phase = if state == State::Planned {
        "upload"
    } else {
        "upload-outcome"
    };
    format!("snapshot:{session}:{phase}")
}

fn signed(keys: &Keys, grant: &Grant, body: Body) -> Result<Event, SdkError> {
    if keys.public_key() != grant.scope().agent {
        return Err(invalid("only the approved agent records snapshot uploads"));
    }
    let mut values = grant.scope().tags(grant.requester());
    values.push(vec!["d".into(), coordinate(body.session, body.state)]);
    let content =
        serde_json::to_string(&body).map_err(|_| invalid("invalid snapshot upload metadata"))?;
    if content.len() > 8192 {
        return Err(invalid("snapshot upload metadata exceeds bound"));
    }
    EventBuilder::new(Kind::from(kind::KIND_SANDBOX_ATTACH as u16), content)
        .tags(tags(values)?)
        .sign_with_keys(keys)
        .map_err(|_| invalid("could not sign snapshot upload metadata"))
}

/// Verified historical upload metadata. It has no NIP-40 expiration: operators
/// can identify ambiguous retained ciphertext after approval expiry or Pod loss.
pub struct Upload {
    event: Event,
    body: Body,
    grant: Grant,
}

impl Upload {
    /// Verify signature, historical owner approval, immutable scope and shape.
    /// The approval is checked at its creation time, not for current liveness.
    pub fn parse(event: &Event, scope: Scope, owner: PublicKey) -> Result<Self, SdkError> {
        if event.kind.as_u16() as u32 != kind::KIND_SANDBOX_ATTACH
            || event.content.len() > 8192
            || event.pubkey != scope.agent
            || event.verify().is_err()
            || event.tags.iter().any(|tag| {
                tag.as_slice()
                    .first()
                    .is_some_and(|key| key == "expiration")
            })
        {
            return Err(invalid("invalid snapshot upload envelope"));
        }
        let body: Body = serde_json::from_str(&event.content)
            .map_err(|_| invalid("invalid snapshot upload metadata"))?;
        let grant = Grant::parse(&body.grant, scope, body.grant.created_at.as_secs())?;
        if body.version != 1
            || body.mode != "snapshot"
            || body.session != grant.session()
            || body.grant.pubkey != owner
            || body.expires_at != grant.expires_at()
            || !scope.matches(event, grant.requester())
            || tag(event, "d") != Some(coordinate(body.session, body.state).as_str())
            || body.sha256.len() != 64
            || !body
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || body.size < (MAGIC.len() + 16) as u64
            || body.size > MAX_BLOB_BYTES as u64
        {
            return Err(invalid("invalid snapshot upload scope or bounds"));
        }
        Ok(Self {
            event: event.clone(),
            body,
            grant,
        })
    }
    /// The exact signed record for idempotent retries.
    pub fn event(&self) -> &Event {
        &self.event
    }
    /// Confirmed, planned or ambiguous upload state; none promises erasure.
    pub fn state(&self) -> State {
        self.body.state
    }
    /// SHA-256 of the ciphertext, never a plaintext file hash.
    pub fn hash(&self) -> &str {
        &self.body.sha256
    }
    /// Exact encrypted object size.
    pub fn size(&self) -> u64 {
        self.body.size
    }
    /// Owner-signed approval metadata; current authority requires fresh checks.
    pub fn grant(&self) -> &Grant {
        &self.grant
    }
    /// Unique acceptance attempt that must have received the atomic first-claim receipt.
    pub fn claim(&self) -> EventId {
        self.body.claim
    }
}

/// Create metadata to confirm durably on the relay BEFORE the ciphertext PUT.
/// Callers must already hold the exact atomic first-claim receipt. No key is stored.
pub fn intent(
    keys: &Keys,
    grant: &Grant,
    claim: &Event,
    sealed: &Sealed,
) -> Result<Event, SdkError> {
    let body: serde_json::Value =
        serde_json::from_str(&claim.content).map_err(|_| invalid("invalid snapshot claim"))?;
    if grant.expires_at() <= Timestamp::now().as_secs()
        || sealed.metadata.grant != grant.id()
        || sealed.metadata.expires_at != grant.expires_at()
        || claim.pubkey != keys.public_key()
        || claim.verify().is_err()
        || buzz_core::sandbox_attach::acceptance_coordinate(claim)
            != Some(("snapshot", grant.session()))
        || !grant.scope().matches(claim, grant.requester())
        || body["grant"]["id"] != serde_json::json!(grant.id())
        || body["mode"] != "snapshot"
        || body["status"] != "accepted"
        || body["bytes"] != 0
    {
        return Err(invalid("snapshot upload requires its accepted claim"));
    }
    signed(
        keys,
        grant,
        Body {
            version: 1,
            mode: "snapshot".into(),
            session: grant.session(),
            state: State::Planned,
            grant: grant.event().clone(),
            claim: claim.id,
            sha256: sealed.hash().to_owned(),
            size: sealed.ciphertext().len() as u64,
            expires_at: grant.expires_at(),
        },
    )
}

/// Record one known or ambiguous outcome. Its separate coordinate preserves the
/// original pre-PUT intent even for same-second writes or a lost acknowledgement.
pub fn outcome(keys: &Keys, planned: &Upload, state: State) -> Result<Event, SdkError> {
    if planned.state() != State::Planned || state == State::Planned {
        return Err(invalid(
            "snapshot upload outcome requires its original plan",
        ));
    }
    signed(
        keys,
        &planned.grant,
        Body {
            state,
            ..planned.body.clone()
        },
    )
}

#[cfg(test)]
mod tests;
