//! Verified completion metadata; not permission to retrieve an artifact.

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u8,
    mode: String,
    session: Uuid,
    status: String,
    grant: Event,
    bytes: u64,
    reason: Option<Failure>,
}

/// Agent-signed completion/failure for one exact owner-approved snapshot.
pub struct End {
    body: Body,
}

impl End {
    /// Verify signature, scope, owner approval and bounded metadata.
    /// Completion may arrive after expiry; this never grants current access.
    pub fn parse(event: &Event, grant: &Grant) -> Result<Self, SdkError> {
        if event.kind.as_u16() as u32 != kind::KIND_SANDBOX_ATTACH
            || event.content.len() > 8192
            || event.pubkey != grant.scope().agent
            || event.verify().is_err()
            || !grant.scope().matches(event, grant.requester())
            || tag(event, "d") != Some(format!("snapshot:{}:ended", grant.session()).as_str())
            || event.tags.iter().any(|tag| {
                tag.as_slice()
                    .first()
                    .is_some_and(|key| key == "expiration")
            })
        {
            return Err(invalid("invalid snapshot completion envelope"));
        }
        let body: Body = serde_json::from_str(&event.content)
            .map_err(|_| invalid("invalid snapshot completion metadata"))?;
        if body.version != 1
            || body.mode != "snapshot"
            || body.status != "ended"
            || body.session != grant.session()
            || body.grant != *grant.event()
            || body.bytes > MAX_BLOB_BYTES as u64
        {
            return Err(invalid("snapshot completion does not match approval"));
        }
        Ok(Self { body })
    }
    /// Public failure category, or None when artifact publication completed.
    pub fn reason(&self) -> Option<Failure> {
        self.body.reason
    }
    /// Ciphertext artifact size, or zero when encryption never completed.
    pub fn bytes(&self) -> u64 {
        self.body.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_binds_exact_agent_approval_scope_and_failure() {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let scope = Scope {
            channel: Uuid::new_v4(),
            thread: EventId::all_zeros(),
            agent: agent.public_key(),
        };
        let now = Timestamp::now().as_secs();
        let request = request(&owner, scope, Uuid::new_v4(), now + 600).unwrap();
        let pending = Pending::parse(&request, now).unwrap();
        let grant =
            Grant::parse(&approve(&owner, &pending, now + 600).unwrap(), scope, now).unwrap();
        let event = audit(&agent, &grant, "ended", 42, Some(Failure::Scan)).unwrap();
        let end = End::parse(&event, &grant).unwrap();
        assert!(matches!(end.reason(), Some(Failure::Scan)));
        assert_eq!(end.bytes(), 42);
        let unsigned = EventBuilder::new(event.kind, event.content.clone())
            .tags(event.tags.clone().to_vec())
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert!(End::parse(&unsigned, &grant).is_err());
        let accepted = audit(&agent, &grant, "accepted", 0, None).unwrap();
        assert!(End::parse(&accepted, &grant).is_err());
        let mut body: serde_json::Value = serde_json::from_str(&event.content).unwrap();
        body["session"] = serde_json::json!(Uuid::new_v4());
        let mismatched = EventBuilder::new(event.kind, body.to_string())
            .tags(event.tags.clone().to_vec())
            .sign_with_keys(&agent)
            .unwrap();
        assert!(End::parse(&mismatched, &grant).is_err());
    }
}
