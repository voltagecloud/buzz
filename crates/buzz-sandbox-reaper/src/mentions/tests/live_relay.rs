//! Run with BUZZ_MENTION_TEST_RELAY pointing at an isolated local Buzz relay.
//! Uses real relay authentication, channel membership and subscription filters.

use super::*;

#[tokio::test]
#[ignore = "requires an isolated local relay with NIP-OA enabled"]
async fn unchanged_client_starts_agents_through_real_relay_permissions() {
    let relay = std::env::var("BUZZ_MENTION_TEST_RELAY")
        .expect("set BUZZ_MENTION_TEST_RELAY to an isolated local relay");
    let url = url::Url::parse(&relay).unwrap();
    assert_eq!(url.scheme(), "ws");
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    tokio::time::timeout(Duration::from_secs(60), exercise(&relay))
        .await
        .expect("real-relay mention workflow timed out");
}

async fn exercise(relay: &str) {
    let owner = keys("33");
    let agent = keys("22");
    let mut phone = NostrWsConnection::connect_authenticated(relay, &owner, None)
        .await
        .unwrap();
    let channel = uuid::Uuid::new_v4();
    for builder in [
        EventBuilder::new(Kind::Custom(30177), "{}").tags([
            Tag::parse(["d", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["runtime", "voltage.remote-workspace/sandbox"]).unwrap(),
        ]),
        buzz_sdk::build_create_channel(
            channel,
            &format!("phone-mention-{channel}"),
            Some(buzz_sdk::Visibility::Private),
            Some(buzz_sdk::ChannelKind::Stream),
            None,
            None,
            false,
        )
        .unwrap(),
        buzz_sdk::build_add_member(channel, &agent.public_key().to_hex(), None).unwrap(),
    ] {
        let result = phone
            .send_event(builder.sign_with_keys(&owner).unwrap())
            .await
            .unwrap();
        assert!(result.accepted, "channel setup refused: {}", result.message);
    }

    let cluster = sandbox_fake::Shared::default();
    register(&cluster, relay).await;
    let stop = CancellationToken::new();
    let server = {
        let manager = manager(&cluster);
        let stop = stop.clone();
        let relay = relay.to_owned();
        tokio::spawn(async move {
            crate::serve::run_services(manager, &relay, None, true, stop.cancelled()).await;
        })
    };
    let mention = |text: &str, root: Option<&Event>| {
        let mut tags = vec![
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::public_key(agent.public_key()),
        ];
        if let Some(root) = root {
            tags.push(Tag::parse(["e", &root.id.to_hex(), "", "reply"]).unwrap());
        }
        EventBuilder::new(Kind::Custom(9), text)
            .tags(tags)
            .sign_with_keys(&owner)
            .unwrap()
    };
    let first = mention("@agent phone task", None);
    let result = phone.send_event(first.clone()).await.unwrap();
    assert!(result.accepted, "phone message refused: {}", result.message);
    wait_for(|| completed(&cluster, &first)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);

    let reply = mention("@agent phone follow-up", Some(&first));
    assert!(phone.send_event(reply.clone()).await.unwrap().accepted);
    wait_for(|| completed(&cluster, &reply)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);

    let second = mention("@agent new phone task", None);
    assert!(phone.send_event(second.clone()).await.unwrap().accepted);
    wait_for(|| completed(&cluster, &second)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 2);
    assert!(inbox(&cluster)["outbox"].as_array().unwrap().is_empty());

    let local = EventBuilder::new(Kind::Custom(30177), "{}")
        .tags([
            Tag::parse(["d", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["runtime", "local/agent"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(Timestamp::now().as_secs() + 1))
        .sign_with_keys(&owner)
        .unwrap();
    let result = phone.send_event(local).await.unwrap();
    assert!(
        result.accepted,
        "runtime update refused: {}",
        result.message
    );
    let after_retype = mention("@agent now run locally", None);
    assert!(
        phone
            .send_event(after_retype.clone())
            .await
            .unwrap()
            .accepted
    );
    wait_for(|| completed(&cluster, &after_retype)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 2);
    assert!(inbox(&cluster)["outbox"].as_array().unwrap().is_empty());

    stop.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}
