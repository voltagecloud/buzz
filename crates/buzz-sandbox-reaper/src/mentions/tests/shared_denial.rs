use super::*;

#[tokio::test]
async fn worker_publishes_the_managers_missing_grant_reason() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let trigger = EventBuilder::new(Kind::Custom(9), "@agent handle this task")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(identity.keys.public_key()),
        ])
        .sign_with_keys(&keys("44"))
        .unwrap();
    {
        let mut events = relay.events.lock().unwrap();
        events.retain(|event| event.kind.as_u16() != 39002);
        events.push(gateway::roster(true));
        events.push(
            EventBuilder::new(Kind::Metadata, "{}")
                .tag(identity.auth.clone())
                .sign_with_keys(&identity.keys)
                .unwrap(),
        );
        events.push(trigger.clone());
    }
    let config = ManagerConfig::parse(&json!({
        "version":1, "shared_tasks":true, "checkpoint_bucket":"fixture-checkpoints",
        "developers":{"dev":{
            "owner_pubkey":keys("33").public_key(), "namespace":NS,
            "profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"
        }}
    }).to_string()).unwrap();
    let manager = Manager::new(
        keys("11"),
        config,
        sandbox_fake::client(&cluster),
        crate::enroll::tests::offline(Default::default()),
    );
    let mut journal = Journal::acquire(sandbox_fake::client(&cluster), &identity, "worker", 0)
        .await
        .unwrap();
    journal.enqueue(trigger.clone()).await.unwrap();
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    wait_for(|| {
        relay.events.lock().unwrap().iter().any(|event| {
            event.content == "Shared task refused: agent is private in this channel"
                && event.pubkey == keys("22").public_key()
                && event
                    .tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["e", &trigger.id.to_hex(), "", "reply"])
                && event.verify().is_ok()
        })
    })
    .await;
    assert!(completed(&cluster, &trigger));
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    task.abort();
    let _ = task.await;
    relay.stop().await;
}
