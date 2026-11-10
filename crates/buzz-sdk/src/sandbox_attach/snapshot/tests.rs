use super::*;
fn fixture() -> (Keys, Keys, Keys, Pending, Grant) {
    let owner = Keys::generate();
    let agent = Keys::generate();
    let relay = Keys::generate();
    let scope = Scope {
        channel: Uuid::new_v4(),
        thread: EventId::all_zeros(),
        agent: agent.public_key(),
    };
    let event = request(
        &owner,
        scope,
        Uuid::new_v4(),
        Timestamp::now().as_secs() + 600,
    )
    .unwrap();
    let pending = Pending::parse(&event, Timestamp::now().as_secs()).unwrap();
    let approval = approve(&owner, &pending, pending.expires_at()).unwrap();
    let grant = Grant::parse(&approval, scope, Timestamp::now().as_secs()).unwrap();
    (owner, agent, relay, pending, grant)
}
#[test]
fn snapshot_approvals_are_separate_capabilities_bound_to_current_authority() {
    let (owner, agent, relay, pending, grant) = fixture();
    let now = Timestamp::now().as_secs();
    assert!(super::super::Pending::parse(&pending.0.event, now).is_err());
    assert!(super::super::Grant::parse(grant.event(), grant.scope(), now).is_err());
    let terminal = super::super::request(&owner, grant.scope(), Uuid::new_v4(), now + 600).unwrap();
    assert!(Pending::parse(&terminal, now).is_err());
    let roster = EventBuilder::new(Kind::from(39002), "")
        .tags([
            Tag::parse(["d", &grant.scope().channel.to_string()]).unwrap(),
            Tag::parse(["p", &owner.public_key().to_hex(), "", "member"]).unwrap(),
            Tag::parse(["p", &agent.public_key().to_hex(), "", "member"]).unwrap(),
        ])
        .sign_with_keys(&relay)
        .unwrap();
    assert!(grant.authorized(
        &pending,
        Authority {
            owner: owner.public_key(),
            relay: relay.public_key(),
            roster: &roster,
            delegations: &[],
            profiles: &[]
        },
        now
    ));
    assert!(!grant.authorized(
        &pending,
        Authority {
            owner: owner.public_key(),
            relay: relay.public_key(),
            roster: &roster,
            delegations: &[],
            profiles: &[]
        },
        grant.expires_at()
    ));
    let mut forged = grant.event().clone();
    forged.pubkey = agent.public_key();
    assert!(Grant::parse(&forged, grant.scope(), now).is_err());
    let self_approved = EventBuilder::new(grant.event().kind, grant.event().content.clone())
        .tags(grant.event().tags.iter().cloned())
        .allow_self_tagging()
        .sign_with_keys(&agent)
        .unwrap();
    let bad = Grant::parse(&self_approved, grant.scope(), Timestamp::now().as_secs()).unwrap();
    assert!(!bad.authorized(
        &pending,
        Authority {
            owner: owner.public_key(),
            relay: relay.public_key(),
            roster: &roster,
            delegations: &[],
            profiles: &[]
        },
        now
    ));
}
#[test]
fn encrypted_snapshot_result_binds_hash_recipient_scope_signature_approval_and_expiry() {
    let (owner, agent, _, _, grant) = fixture();
    let bytes = b"archive with confidential workspace bytes";
    let sealed = seal(&grant, bytes).unwrap();
    assert_ne!(sealed.ciphertext(), bytes);
    let result = result(&agent, &grant, &sealed).unwrap();
    assert!(!result.content.contains("confidential"));
    let now = Timestamp::now().as_secs();
    let received = receipt(&owner, &grant, &result, now).unwrap();
    assert_eq!(received.hash(), sealed.hash());
    assert_eq!(received.open(sealed.ciphertext(), now).unwrap(), bytes);
    let mut wrong_hash = seal(&grant, bytes).unwrap();
    wrong_hash.metadata.sha256 = "0".repeat(64);
    let bad_result = super::result(&agent, &grant, &wrong_hash).unwrap();
    let bad_receipt = receipt(&owner, &grant, &bad_result, Timestamp::now().as_secs()).unwrap();
    assert!(
        bad_receipt.open(wrong_hash.ciphertext(), now).is_err(),
        "valid AEAD bytes with mismatched signed hash must be refused"
    );
    let mut wrong_size = seal(&grant, bytes).unwrap();
    wrong_size.metadata.size += 1;
    let bad_result = super::result(&agent, &grant, &wrong_size).unwrap();
    let bad_receipt = receipt(&owner, &grant, &bad_result, Timestamp::now().as_secs()).unwrap();
    assert!(bad_receipt.open(wrong_size.ciphertext(), now).is_err());
    let mut oversized = seal(&grant, bytes).unwrap();
    oversized.metadata.size = MAX_BLOB_BYTES as u64 + 1;
    let bad_result = super::result(&agent, &grant, &oversized).unwrap();
    assert!(receipt(&owner, &grant, &bad_result, Timestamp::now().as_secs()).is_err());
    let mut corrupted = sealed.ciphertext().to_vec();
    corrupted[20] ^= 1;
    assert!(received.open(&corrupted, now).is_err());
    assert!(received
        .open(&corrupted[..corrupted.len() - 1], now)
        .is_err());
    assert!(received
        .open(sealed.ciphertext(), grant.expires_at())
        .is_err());
    assert!(receipt(&Keys::generate(), &grant, &result, now).is_err());
    let mut forged = result.clone();
    forged.sig = request(&Keys::generate(), grant.scope(), Uuid::new_v4(), now + 60)
        .unwrap()
        .sig;
    assert!(receipt(&owner, &grant, &forged, now).is_err());
    let (_, _, _, _, other) = fixture();
    assert!(receipt(&owner, &other, &result, now).is_err());
    let mut corrupt_scope = result.clone();
    corrupt_scope
        .tags
        .push(Tag::parse(["h", &Uuid::new_v4().to_string()]).unwrap());
    assert!(receipt(&owner, &grant, &corrupt_scope, now).is_err());
    let first_claim = audit(&agent, &grant, "accepted", 0, None).unwrap();
    let second_claim = audit(&agent, &grant, "accepted", 0, None).unwrap();
    assert_ne!(
        tag(&first_claim, "buzz:attach-claim"),
        tag(&second_claim, "buzz:attach-claim")
    );
    assert!(tag(&first_claim, "expiration").is_none());
    let audit = audit(
        &agent,
        &grant,
        "ended",
        sealed.ciphertext().len() as u64,
        None,
    )
    .unwrap();
    assert!(
        !audit.content.contains("confidential")
            && !audit.content.contains("nonce")
            && !audit.content.contains("\"key\":")
    );
}
