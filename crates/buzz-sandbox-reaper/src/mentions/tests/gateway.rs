use super::*;

fn signed_mention(signer: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
        ])
        .sign_with_keys(signer)
        .unwrap()
}

fn delegation(enabled: bool, age: u64) -> Event {
    let now = Timestamp::now().as_secs();
    let gateway = keys("44").public_key();
    let builder = if enabled {
        buzz_sdk::gateway::Delegation::grant(gateway, now + 3_600, now - age)
    } else {
        buzz_sdk::gateway::Delegation::revoke(gateway, now - age)
    };
    builder.unwrap().sign_with_keys(&keys("33")).unwrap()
}

fn roster(gateway: bool) -> Event {
    let mut tags = vec![Tag::parse(["d", CHANNEL]).unwrap()];
    for key in ["22", "33"].into_iter().chain(gateway.then_some("44")) {
        tags.push(Tag::parse(["p", &keys(key).public_key().to_hex(), "", "member"]).unwrap());
    }
    EventBuilder::new(Kind::Custom(39002), "")
        .tags(tags)
        .sign_with_keys(&keys("11"))
        .unwrap()
}

/// Mention launch from a delegated gateway runs exactly like the owner's own
/// mention; an undelegated key never launches; a revocation stored after the
/// listener's snapshot is still enforced at launch time with a visible reply.
/// Mutations: drop the gateway authors from the listener filter or the
/// gateway branch in `eligible` → the first launch never happens; drop the
/// launch-time delegation check → the revoked mention launches a second
/// sandbox; let `Start::delegated` skip the signer check → stranger launches.
#[tokio::test]
async fn delegated_gateway_mentions_launch_until_revoked() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    register(&cluster, &relay.url).await;
    relay.events.lock().unwrap().push(delegation(true, 10));
    let stop = CancellationToken::new();
    let server = {
        let manager = manager(&cluster);
        let stop = stop.clone();
        let url = relay.url.clone();
        tokio::spawn(async move {
            crate::serve::run_services(manager, &url, None, true, stop.cancelled()).await
        })
    };
    relay.wait_subscription("mentions-history").await;

    let mut gateway = NostrWsConnection::connect_authenticated(&relay.url, &keys("44"), None)
        .await
        .unwrap();
    let mut stranger = NostrWsConnection::connect_authenticated(&relay.url, &keys("55"), None)
        .await
        .unwrap();
    let undelegated = signed_mention(&keys("55"), "@agent run as a stranger");
    assert!(
        stranger
            .send_event(undelegated.clone())
            .await
            .unwrap()
            .accepted
    );
    let missing_member = signed_mention(
        &keys("44"),
        "@agent I have a grant but no channel membership",
    );
    assert!(
        gateway
            .send_event(missing_member.clone())
            .await
            .unwrap()
            .accepted
    );
    wait_for(|| {
        relay.events.lock().unwrap().iter().any(|event| {
            event.content.contains("not a current posting member")
                && event
                    .tags
                    .iter()
                    .any(|tag| tag.as_slice().get(1) == Some(&missing_member.id.to_hex()))
        })
    })
    .await;
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    relay
        .events
        .lock()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 39002);
    relay.events.lock().unwrap().push(roster(true));
    let first = signed_mention(&keys("44"), "@agent run from the gateway");
    assert!(gateway.send_event(first.clone()).await.unwrap().accepted);
    wait_for(|| completed(&cluster, &first)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    assert!(!completed(&cluster, &undelegated), "stranger was journaled");

    // Removing channel membership revokes launch independently of the grant.
    relay
        .events
        .lock()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 39002);
    relay.events.lock().unwrap().push(roster(false));
    let removed = signed_mention(&keys("44"), "@agent run after membership removal");
    assert!(gateway.send_event(removed.clone()).await.unwrap().accepted);
    wait_for(|| {
        relay.events.lock().unwrap().iter().any(|event| {
            event.content.contains("not a current posting member")
                && event
                    .tags
                    .iter()
                    .any(|tag| tag.as_slice().get(1) == Some(&removed.id.to_hex()))
        })
    })
    .await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    relay
        .events
        .lock()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 39002);
    relay.events.lock().unwrap().push(roster(true));

    // Revoke after the listener captured its gateway snapshot.
    relay.events.lock().unwrap().push(delegation(false, 5));
    let revoked = signed_mention(&keys("44"), "@agent run after revocation");
    assert!(gateway.send_event(revoked.clone()).await.unwrap().accepted);
    wait_for(|| {
        relay.events.lock().unwrap().iter().any(|event| {
            event.pubkey == keys("22").public_key()
                && event.content.contains("not currently delegated")
                && event
                    .tags
                    .iter()
                    .any(|tag| tag.as_slice().get(1) == Some(&revoked.id.to_hex()))
        })
    })
    .await;
    assert_eq!(
        cluster.lock().unwrap().sandboxes.len(),
        1,
        "a revoked gateway must not launch"
    );
    stop.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
    relay.stop().await;
}
