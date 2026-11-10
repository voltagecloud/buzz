use super::*;

fn workflow(signer: &Keys, gateway: PublicKey, agent: PublicKey, channel: uuid::Uuid) -> Event {
    EventBuilder::new(Kind::Custom(9), "@agent run the gateway workflow")
        .tags([
            nostr::Tag::parse(["h", &channel.to_string()]).unwrap(),
            nostr::Tag::public_key(agent),
            nostr::Tag::parse(["buzz:workflow", "true"]).unwrap(),
            nostr::Tag::parse(["buzz:workflow-owner", &gateway.to_hex()]).unwrap(),
            nostr::Tag::parse(["buzz:workflow-mention", &agent.to_hex()]).unwrap(),
        ])
        .sign_with_keys(signer)
        .unwrap()
}

/// Exercise both production admission boundaries. Removing provenance, scope,
/// current delegation or membership checks admits one of the negative cases.
#[tokio::test]
async fn gateway_workflows_require_provenance_scope_and_live_authority() {
    let relay = fake_relay().await;
    let (owner, agent, gateway) = (Keys::generate(), Keys::generate(), Keys::generate());
    let relay_key = Keys::parse(&"11".repeat(32)).unwrap();
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    let access = Arc::new(GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    ));
    let mut gate = crate::InboundAuthorGate::connect(
        &relay.rest,
        &agent.public_key().to_hex(),
        "workflow test",
    )
    .await;
    gate.enable_gateways(Some(access.clone()));
    let owner_cache = crate::OwnerCache::new(Some(owner.public_key().to_hex()));
    let allowlist = std::collections::HashSet::new();
    let mut ctx = crate::pool::tests::make_prompt_context_no_owner();
    ctx.agent_owner_pubkey = Some(owner.public_key());
    ctx.gateways = Some(access.clone());
    let check = |event: Event, expected: bool| {
        let (gate, cache, allowlist, ctx, rest) =
            (&gate, &owner_cache, &allowlist, &ctx, &relay.rest);
        let gateway = gateway.public_key();
        async move {
            let decision = gate
                .evaluate_for_test(
                    &event,
                    &crate::RespondTo::Owner,
                    allowlist,
                    false,
                    cache,
                    rest,
                )
                .await;
            assert_eq!(decision.allowed, expected, "ingress {}", event.id);
            if expected {
                assert_eq!(decision.effective_author, gateway.to_hex());
            }
            let batch = crate::queue::FlushBatch {
                channel_id: channel(),
                scope: crate::scope::SessionScope::Thread {
                    channel_id: channel(),
                    root_event_id: event.id.to_hex(),
                },
                events: vec![crate::queue::BatchEvent {
                    event,
                    prompt_tag: "@mention".into(),
                    received_at: std::time::Instant::now(),
                    edit: None,
                }],
                cancelled_events: vec![],
                cancel_reason: None,
            };
            assert_eq!(
                crate::pool::dispatch_refusal(ctx, &batch).await.is_none(),
                expected,
                "dispatch"
            );
        }
    };
    let valid = workflow(
        &relay_key,
        gateway.public_key(),
        agent.public_key(),
        channel(),
    );
    check(valid.clone(), true).await;
    check(
        workflow(
            &Keys::generate(),
            gateway.public_key(),
            agent.public_key(),
            channel(),
        ),
        false,
    )
    .await;
    check(
        workflow(
            &relay_key,
            gateway.public_key(),
            agent.public_key(),
            uuid::Uuid::new_v4(),
        ),
        false,
    )
    .await;
    check(
        workflow(
            &relay_key,
            gateway.public_key(),
            Keys::generate().public_key(),
            channel(),
        ),
        false,
    )
    .await;
    let mut tampered = valid.clone();
    tampered.content.push_str(" forged");
    check(tampered, false).await;

    relay.admit(&[owner.public_key(), agent.public_key()]);
    skip(VERDICT_TTL).await;
    check(valid.clone(), false).await;
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    skip(VERDICT_TTL).await;
    check(valid.clone(), true).await;

    let now = unix_now();
    *relay.heads.lock().unwrap() =
        vec![
            buzz_sdk::gateway::Delegation::grant(gateway.public_key(), now, now - 10)
                .unwrap()
                .sign_with_keys(&owner)
                .unwrap(),
        ];
    skip(VERDICT_TTL).await;
    check(valid.clone(), false).await;
    *relay.heads.lock().unwrap() = vec![
        grant(&owner, gateway.public_key(), 10),
        revoke(&owner, gateway.public_key(), 5),
    ];
    skip(VERDICT_TTL).await;
    check(valid, false).await;
}
