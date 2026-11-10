use super::*;

fn workflow(content: &str, mentioned: nostr::PublicKey) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
            Tag::parse(["buzz:workflow", "true"]).unwrap(),
            Tag::parse(["buzz:workflow-owner", &keys("44").public_key().to_hex()]).unwrap(),
            Tag::parse(["buzz:workflow-mention", &mentioned.to_hex()]).unwrap(),
        ])
        .sign_with_keys(&keys("11"))
        .unwrap()
}

/// Drive the real listener, fresh delivery gate and manager launch boundary.
/// A relay signature alone does not authorize an implicit workflow mention;
/// a valid gateway-owned workflow launches, and later revocation still wins.
#[tokio::test]
async fn gateway_owned_workflow_launches_only_with_live_proven_authority() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    register(&cluster, &relay.url).await;
    relay
        .events
        .lock()
        .unwrap()
        .push(gateway::delegation(true, 10));
    relay
        .events
        .lock()
        .unwrap()
        .retain(|e| e.kind.as_u16() != 39002);
    relay.events.lock().unwrap().push(gateway::roster(true));
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
    let mut workflow_sender =
        NostrWsConnection::connect_authenticated(&relay.url, &keys("11"), None)
            .await
            .unwrap();
    let forged = workflow(
        "@agent implicit p tag is insufficient",
        keys("55").public_key(),
    );
    assert!(
        workflow_sender
            .send_event(forged.clone())
            .await
            .unwrap()
            .accepted
    );
    let valid = workflow("@agent explicit gateway workflow", keys("22").public_key());
    assert!(
        workflow_sender
            .send_event(valid.clone())
            .await
            .unwrap()
            .accepted
    );
    wait_for(|| completed(&cluster, &valid)).await;
    assert!(
        !completed(&cluster, &forged),
        "invalid provenance was journaled"
    );
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);

    relay
        .events
        .lock()
        .unwrap()
        .push(gateway::delegation(false, 5));
    let revoked = workflow(
        "@agent workflow after gateway revoked",
        keys("22").public_key(),
    );
    assert!(
        workflow_sender
            .send_event(revoked.clone())
            .await
            .unwrap()
            .accepted
    );
    wait_for(|| {
        relay.events.lock().unwrap().iter().any(|event| {
            event.content.contains("not currently delegated")
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
        "revoked workflow launched"
    );
    stop.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
    relay.stop().await;
}
