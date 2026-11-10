use super::*;

struct Fixture {
    owner: Keys,
    agent: Keys,
    grant: Grant,
    claim: Event,
    sealed: Sealed,
}
fn fixture() -> Fixture {
    let (owner, agent) = (Keys::generate(), Keys::generate());
    let scope = Scope {
        channel: Uuid::new_v4(),
        thread: EventId::all_zeros(),
        agent: agent.public_key(),
    };
    let now = Timestamp::now().as_secs();
    let request = super::super::request(&owner, scope, Uuid::new_v4(), now + 600).unwrap();
    let pending = super::super::Pending::parse(&request, now).unwrap();
    let approval = super::super::approve(&owner, &pending, now + 600).unwrap();
    let grant = Grant::parse(&approval, scope, now).unwrap();
    let claim = super::super::audit(&agent, &grant, "accepted", 0, None).unwrap();
    let sealed = super::super::seal(&grant, b"scanned archive fixture").unwrap();
    Fixture {
        owner,
        agent,
        grant,
        claim,
        sealed,
    }
}

#[test]
fn upload_plan_and_outcome_retain_exact_public_metadata_without_a_key_or_expiration() {
    let f = fixture();
    let event = intent(&f.agent, &f.grant, &f.claim, &f.sealed).unwrap();
    let planned = Upload::parse(&event, f.grant.scope(), f.owner.public_key()).unwrap();
    assert_eq!(planned.state(), State::Planned);
    assert_eq!(planned.hash(), f.sealed.hash());
    assert_eq!(planned.size(), f.sealed.ciphertext().len() as u64);
    assert_eq!(planned.claim(), f.claim.id);
    assert_eq!(planned.grant().id(), f.grant.id());
    let body: serde_json::Value = serde_json::from_str(&event.content).unwrap();
    assert!(body.get("key").is_none() && body.get("nonce").is_none());
    assert!(tag(&event, "expiration").is_none());
    for state in [State::Uploaded, State::Unconfirmed] {
        let result = outcome(&f.agent, &planned, state).unwrap();
        let parsed = Upload::parse(&result, f.grant.scope(), f.owner.public_key()).unwrap();
        assert_eq!(parsed.state(), state);
        assert_eq!(parsed.hash(), planned.hash());
        assert_eq!(parsed.claim(), planned.claim());
        assert_ne!(
            tag(&result, "d"),
            tag(&event, "d"),
            "outcome replaced its durable pre-PUT intent"
        );
        assert!(tag(&result, "expiration").is_none());
        assert!(outcome(&f.agent, &parsed, State::Uploaded).is_err());
    }
    assert!(outcome(&f.owner, &planned, State::Uploaded).is_err());
    assert!(outcome(&f.agent, &planned, State::Planned).is_err());
}

#[test]
fn upload_metadata_rejects_wrong_owner_scope_claim_ciphertext_and_expiring_records() {
    let f = fixture();
    let event = intent(&f.agent, &f.grant, &f.claim, &f.sealed).unwrap();
    assert!(Upload::parse(&event, f.grant.scope(), Keys::generate().public_key()).is_err());
    let wrong = Scope {
        channel: Uuid::new_v4(),
        ..f.grant.scope()
    };
    assert!(Upload::parse(&event, wrong, f.owner.public_key()).is_err());
    assert!(intent(&f.owner, &f.grant, &f.claim, &f.sealed).is_err());
    let other = fixture();
    assert!(intent(&f.agent, &f.grant, &other.claim, &f.sealed).is_err());
    assert!(intent(&f.agent, &f.grant, &f.claim, &other.sealed).is_err());
    let expiring = EventBuilder::new(event.kind, event.content.clone())
        .tags(event.tags.clone().to_vec())
        .tag(Tag::expiration(
            Timestamp::now() + std::time::Duration::from_secs(10),
        ))
        .sign_with_keys(&f.agent)
        .unwrap();
    assert!(Upload::parse(&expiring, f.grant.scope(), f.owner.public_key()).is_err());
    let mut body: serde_json::Value = serde_json::from_str(&event.content).unwrap();
    body["size"] = serde_json::json!(MAX_BLOB_BYTES + 1);
    let invalid = EventBuilder::new(event.kind, body.to_string())
        .tags(event.tags.clone().to_vec())
        .sign_with_keys(&f.agent)
        .unwrap();
    assert!(Upload::parse(&invalid, f.grant.scope(), f.owner.public_key()).is_err());
}
