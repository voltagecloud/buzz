use super::*;

fn authority<'a>(
    owner: PublicKey,
    relay: PublicKey,
    roster: &'a Event,
    delegations: &'a [Event],
) -> Authority<'a> {
    Authority {
        owner,
        relay,
        roster,
        delegations,
        profiles: &[],
    }
}

fn scope(agent: &Keys) -> Scope {
    Scope {
        channel: Uuid::new_v4(),
        thread: EventId::all_zeros(),
        agent: agent.public_key(),
    }
}

#[test]
fn terminal_grants_fence_scope_owner_gateway_and_expiry() {
    let owner = Keys::generate();
    let gateway = Keys::generate();
    let agent = Keys::generate();
    let outsider = Keys::generate();
    let scope = scope(&agent);
    let now = Timestamp::now().as_secs();
    let relay = Keys::generate();
    let roster = EventBuilder::new(Kind::from(39002), "")
        .tags([
            Tag::parse(["d", scope.channel.to_string().as_str()]).unwrap(),
            Tag::parse(["p", owner.public_key().to_hex().as_str(), "", "owner"]).unwrap(),
            Tag::parse(["p", agent.public_key().to_hex().as_str(), "", "bot"]).unwrap(),
            Tag::parse(["p", gateway.public_key().to_hex().as_str(), "", "member"]).unwrap(),
        ])
        .sign_with_keys(&relay)
        .unwrap();
    let pending_event = request(&owner, scope, Uuid::new_v4(), now + 60).unwrap();
    let pending = Pending::parse(&pending_event, now).unwrap();
    assert!(
        Grant::parse(&pending_event, scope, now).is_err(),
        "a request is never authority"
    );
    let event = approve(&owner, &pending, now + 60).unwrap();
    let grant = Grant::parse(&event, scope, now).unwrap();
    assert_eq!(grant.request_id(), pending.id());
    assert!(!grant.authorized(
        &pending,
        authority(owner.public_key(), relay.public_key(), &roster, &[]),
        grant.expires_at()
    ));
    let old_request = EventBuilder::new(pending_event.kind, pending_event.content.clone())
        .tags(pending_event.tags.to_vec())
        .custom_created_at(Timestamp::from_secs(pending_event.created_at.as_secs() - 1))
        .sign_with_keys(&owner)
        .unwrap();
    let old_request = Pending::parse(&old_request, now).unwrap();
    assert!(!grant.authorized(
        &old_request,
        authority(owner.public_key(), relay.public_key(), &roster, &[]),
        now
    ));
    let mut bad_signature = event.clone();
    bad_signature.sig = request(&outsider, scope, Uuid::new_v4(), now + 60)
        .unwrap()
        .sig;
    assert!(Grant::parse(&bad_signature, scope, now).is_err());
    assert!(grant.authorized(
        &pending,
        authority(owner.public_key(), relay.public_key(), &roster, &[]),
        now
    ));
    assert!(Grant::parse(&event, scope, now + 60).is_err());
    assert!(Grant::parse(
        &event,
        Scope {
            channel: Uuid::new_v4(),
            ..scope
        },
        now
    )
    .is_err());
    assert!(request(&owner, scope, Uuid::new_v4(), now + MAX_GRANT_SECS + 60).is_err());
    let delegated_event = request(&gateway, scope, Uuid::new_v4(), now + 60).unwrap();
    let delegated_pending = Pending::parse(&delegated_event, now).unwrap();
    let head =
        crate::gateway::build_gateway_grant(&owner, gateway.public_key(), now + 120).unwrap();
    let self_approved = approve(&gateway, &delegated_pending, now + 60).unwrap();
    let self_approved = Grant::parse(&self_approved, scope, now).unwrap();
    assert!(
        !self_approved.authorized(
            &delegated_pending,
            authority(
                owner.public_key(),
                relay.public_key(),
                &roster,
                std::slice::from_ref(&head)
            ),
            now
        ),
        "live gateways cannot authorize their own shell"
    );
    let delegated = approve(&owner, &delegated_pending, now + 60).unwrap();
    let delegated = Grant::parse(&delegated, scope, now).unwrap();
    assert!(!delegated.authorized(
        &delegated_pending,
        authority(owner.public_key(), relay.public_key(), &roster, &[]),
        now
    ));
    assert!(delegated.authorized(
        &delegated_pending,
        authority(
            owner.public_key(),
            relay.public_key(),
            &roster,
            std::slice::from_ref(&head)
        ),
        now
    ));
    assert!(!delegated.authorized(
        &delegated_pending,
        authority(
            outsider.public_key(),
            relay.public_key(),
            &roster,
            std::slice::from_ref(&head)
        ),
        now
    ));
    let without_gateway = EventBuilder::new(Kind::from(39002), "")
        .tags(
            roster
                .tags
                .iter()
                .filter(|tag| tag.as_slice().get(1) != Some(&gateway.public_key().to_hex()))
                .cloned(),
        )
        .sign_with_keys(&relay)
        .unwrap();
    assert!(!delegated.authorized(
        &delegated_pending,
        authority(
            owner.public_key(),
            relay.public_key(),
            &without_gateway,
            std::slice::from_ref(&head)
        ),
        now
    ));
    assert!(!delegated.authorized(
        &delegated_pending,
        authority(
            owner.public_key(),
            outsider.public_key(),
            &roster,
            std::slice::from_ref(&head)
        ),
        now
    ));
    let mut forged = event.clone();
    forged.content = forged.content.replace("terminal", "snapshot");
    assert!(Grant::parse(&forged, scope, now).is_err());
    let audit = audit(&agent, &grant, "ended", 12, 34).unwrap();
    assert!(Grant::parse(&audit, scope, now).is_err());
    assert!(!audit.content.contains("keystrokes"));
}

#[test]
fn encrypted_frames_bind_signatures_recipients_scope_grant_and_freshness() {
    let client = Keys::generate();
    let agent = Keys::generate();
    let other = Keys::generate();
    let scope = scope(&agent);
    let grant = EventId::all_zeros();
    let mut flow = Flow::new(grant);
    let frame = flow
        .send(Packet::data(b"private terminal bytes").unwrap(), 0)
        .unwrap();
    let event = encrypt(&client, agent.public_key(), scope, &frame).unwrap();
    let now = event.created_at.as_secs();
    assert!(!event.content.contains("private terminal bytes"));
    assert!(!format!("{frame:?}").contains(&STANDARD.encode(b"private terminal bytes")));
    assert_eq!(
        decrypt(&agent, client.public_key(), scope, grant, &event, now).unwrap(),
        frame
    );
    assert!(decrypt(&other, client.public_key(), scope, grant, &event, now).is_err());
    assert!(decrypt(&agent, other.public_key(), scope, grant, &event, now).is_err());
    assert!(decrypt(
        &agent,
        client.public_key(),
        scope,
        EventId::from_hex(&"a".repeat(64)).unwrap(),
        &event,
        now
    )
    .is_err());
    assert!(decrypt(
        &agent,
        client.public_key(),
        Scope {
            channel: Uuid::new_v4(),
            ..scope
        },
        grant,
        &event,
        now
    )
    .is_err());
    assert!(decrypt(
        &agent,
        client.public_key(),
        scope,
        grant,
        &event,
        now + MAX_FRAME_AGE_SECS + 1
    )
    .is_err());
    assert!(Packet::data(&vec![0; MAX_DATA_BYTES + 1]).is_err());
    assert!(Packet::Data {
        data: STANDARD.encode(vec![0; MAX_DATA_BYTES + 1])
    }
    .bytes()
    .is_err());
    let mut forged_signature = event.clone();
    forged_signature.sig = EventBuilder::new(Kind::TextNote, "other event")
        .sign_with_keys(&other)
        .unwrap()
        .sig;
    assert!(decrypt(
        &agent,
        client.public_key(),
        scope,
        grant,
        &forged_signature,
        now
    )
    .is_err());
    assert!(Packet::Resize { rows: 0, cols: 80 }.bytes().is_err());
    let mut forged = event;
    forged.content.push('A');
    assert!(decrypt(&agent, client.public_key(), scope, grant, &forged, now).is_err());
}

#[test]
fn transport_delivers_once_detects_gaps_bounds_window_and_ends_retry() {
    let grant = EventId::all_zeros();
    let mut sender = Flow::new(grant);
    let mut receiver = Flow::new(grant);
    let a = sender.send(Packet::Open { rows: 24, cols: 80 }, 0).unwrap();
    let b = sender.send(Packet::data(b"one").unwrap(), 0).unwrap();
    assert!(receiver.receive(b.clone()).unwrap().is_none()); // Gap; sender retries.
    assert!(receiver.receive(a.clone()).unwrap().is_some());
    assert!(receiver.receive(a).unwrap().is_none()); // Never repeat a keystroke.
    assert!(receiver.receive(b).unwrap().is_some());
    sender.receive(receiver.acknowledgement()).unwrap();
    for _ in 0..MAX_IN_FLIGHT {
        sender
            .send(Packet::data(&vec![1; MAX_DATA_BYTES]).unwrap(), 0)
            .unwrap();
    }
    assert!(!sender.writable());
    assert!(sender.send(Packet::Close, 0).is_err());
    for retry in 1..=MAX_RETRIES {
        assert_eq!(
            sender.retries(u64::from(retry) * RETRY_MS).unwrap().len(),
            MAX_IN_FLIGHT
        );
    }
    assert!(sender
        .retries((u64::from(MAX_RETRIES) + 1) * RETRY_MS)
        .is_err());
    let mut ack = receiver.acknowledgement();
    ack.ack = u64::MAX;
    assert!(sender.receive(ack).is_err());
}

#[test]
fn terminal_frame_rate_is_bounded_including_acknowledgements() {
    let mut rate = Rate::default();
    for _ in 0..MAX_FRAMES_PER_SECOND {
        assert!(rate.allow(0));
    }
    assert!(!rate.allow(999));
    assert!(rate.allow(1000));
}

#[test]
fn accepted_audits_have_unique_claims_without_expiration() {
    let (owner, agent) = (Keys::generate(), Keys::generate());
    let scope = scope(&agent);
    let now = Timestamp::now().as_secs();
    let pending = Pending::parse(
        &request(&owner, scope, Uuid::new_v4(), now + 60).unwrap(),
        now,
    )
    .unwrap();
    let grant = Grant::parse(&approve(&owner, &pending, now + 60).unwrap(), scope, now).unwrap();
    let first = audit(&agent, &grant, "accepted", 0, 0).unwrap();
    let second = audit(&agent, &grant, "accepted", 0, 0).unwrap();
    let a = tag(&first, "buzz:attach-claim").unwrap();
    let b = tag(&second, "buzz:attach-claim").unwrap();
    assert_ne!(a, b);
    assert_eq!(Uuid::parse_str(a).unwrap().to_string(), a);
    assert_ne!(first.id, second.id);
    assert!(tag(&first, "expiration").is_none());
}
