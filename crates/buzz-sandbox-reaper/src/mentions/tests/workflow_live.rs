//! The real relay scheduler signs the trigger; neither owner nor desktop sends
//! a wake request. The manager and shipped harness must both authorize it.

use super::*;

#[tokio::test]
#[ignore = "requires isolated local relay and built buzz-acp/buzz binaries"]
async fn scheduled_workflow_wakes_private_remote_agent_with_owner_offline() {
    let relay = std::env::var("BUZZ_MENTION_TEST_RELAY").unwrap();
    let url = url::Url::parse(&relay).unwrap();
    assert_eq!(url.scheme(), "ws");
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    let acp = PathBuf::from(std::env::var("BUZZ_SHARING_TEST_ACP").unwrap());
    let cli = PathBuf::from(std::env::var("BUZZ_SHARING_TEST_CLI").unwrap());
    assert!(acp.is_file() && cli.is_file());
    tokio::time::timeout(Duration::from_secs(150), exercise(&relay, &acp, &cli))
        .await
        .expect("scheduled remote-agent workflow timed out");
}

async fn exercise(relay: &str, acp: &Path, cli: &Path) {
    let (owner, observer, agent) = (keys("33"), keys("44"), keys("22"));
    let channel = uuid::Uuid::new_v4();
    let mut phone = NostrWsConnection::connect_authenticated(relay, &owner, None)
        .await
        .unwrap();
    for member in [observer.public_key(), keys("11").public_key()] {
        publish(
            &mut phone,
            EventBuilder::new(Kind::Custom(9030), "").tag(Tag::public_key(member)),
            &owner,
        )
        .await;
    }
    publish(&mut phone, EventBuilder::new(Kind::Metadata, "{}"), &owner).await;
    publish(
        &mut phone,
        buzz_sdk::build_create_channel(
            channel,
            &format!("scheduled-{channel}"),
            Some(buzz_sdk::Visibility::Private),
            Some(buzz_sdk::ChannelKind::Stream),
            None,
            None,
            false,
        )
        .unwrap(),
        &owner,
    )
    .await;
    for (member, role) in [
        (observer.public_key(), None),
        (agent.public_key(), Some(buzz_sdk::MemberRole::Bot)),
    ] {
        publish(
            &mut phone,
            buzz_sdk::build_add_member(channel, &member.to_hex(), role).unwrap(),
            &owner,
        )
        .await;
    }
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    let tag = buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap();
    let mut agent_socket = NostrWsConnection::connect_authenticated(relay, &agent, Some(&tag))
        .await
        .unwrap();
    publish(
        &mut agent_socket,
        EventBuilder::new(Kind::Metadata, r#"{"name":"ScheduledBrokkr"}"#).tag(tag),
        &agent,
    )
    .await;
    drop(agent_socket);
    publish(
        &mut phone,
        EventBuilder::new(Kind::Custom(30177), "{}").tags([
            Tag::parse(["d", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["runtime", "voltage.remote-workspace/sandbox"]).unwrap(),
        ]),
        &owner,
    )
    .await;
    // Deliberately no sharing grant: an owner's workflow can use their private
    // agent. Teammate workflows still require the existing current grant.
    let cluster = sandbox_fake::Shared::default();
    register(&cluster, relay).await;
    let config = ManagerConfig::parse(&json!({"version":1,"shared_tasks":true,
        "checkpoint_bucket":"fixture-checkpoints","developers":{"dev":{
        "owner_pubkey":owner.public_key(),"namespace":NS,
        "profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"
    }}}).to_string()).unwrap();
    let manager = Arc::new(
        Manager::new(
            keys("11"),
            config,
            sandbox_fake::client(&cluster),
            crate::enroll::tests::offline(Default::default()),
        )
        .with_sandbox_defaults(
            json!({"image":format!("example.invalid/agent@sha256:{}","a".repeat(64)),
            "pod_options":{"active_deadline_seconds":3600}})
            .as_object()
            .unwrap()
            .clone(),
        ),
    );
    let cancel = CancellationToken::new();
    let server = {
        let cancel = cancel.clone();
        let relay = relay.to_owned();
        tokio::spawn(async move {
            crate::serve::run_services(manager, &relay, None, true, cancel.cancelled()).await;
        })
    };
    wait_for(|| !inbox(&cluster).is_null()).await;
    let workflow_id = uuid::Uuid::new_v4();
    let yaml = "name: Scheduled remote agent\ntrigger:\n  on: schedule\n  cron: '* * * * *'\nsteps:\n  - id: work\n    action: send_message\n    text: '@ScheduledBrokkr scheduled fixture task'\n";
    let definition = publish(
        &mut phone,
        buzz_sdk::build_workflow_def(channel, workflow_id, yaml).unwrap(),
        &owner,
    )
    .await;
    drop(phone);
    let mut observer_socket = NostrWsConnection::connect_authenticated(relay, &observer, None)
        .await
        .unwrap();
    let prompt = tokio::time::timeout(Duration::from_secs(75), async {
        loop {
            if let Some(event) = channel_events(&mut observer_socket, channel)
                .await
                .into_iter()
                .find(|event| event.content.contains("scheduled fixture task"))
            {
                return event;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("relay scheduler did not fire");
    assert_ne!(prompt.pubkey, owner.public_key());
    assert_eq!(
        buzz_sdk::workflow::owner(&prompt, prompt.pubkey, agent.public_key()),
        Some(owner.public_key())
    );
    // Stop future fires before checking the harness, including on later failure.
    let mut phone = NostrWsConnection::connect_authenticated(relay, &owner, None)
        .await
        .unwrap();
    publish(
        &mut phone,
        buzz_sdk::build_workflow_update(
            channel,
            workflow_id,
            &format!("enabled: false\n{yaml}"),
            &definition.id.to_hex(),
        )
        .unwrap(),
        &owner,
    )
    .await;
    drop(phone);
    wait_for(|| completed(&cluster, &prompt)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    {
        let cluster = cluster.lock().unwrap();
        let sandbox = cluster.sandboxes.values().next().unwrap();
        let annotations = &sandbox["metadata"]["annotations"];
        assert_eq!(
            annotations[buzz_backend_kubernetes::lifecycle::CHANNEL],
            channel.to_string()
        );
        assert_eq!(
            annotations[buzz_backend_kubernetes::lifecycle::THREAD],
            prompt.id.to_hex()
        );
    }
    let directory = tempfile::Builder::new()
        .prefix("buzz-scheduled-agent-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("Scheduled harness evidence: {}", directory.display());
    let process = harness(
        (acp, cli),
        &directory,
        relay,
        (&owner, &agent),
        &prompt,
        None,
    );
    let reply = wait_message(
        &mut observer_socket,
        channel,
        agent.public_key(),
        "shared fixture completed",
    )
    .await;
    let ancestry: Vec<_> = reply
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| {
            tag.first().map(String::as_str) == Some("e")
                && matches!(tag.get(3).map(String::as_str), Some("root" | "reply"))
        })
        .collect();
    assert!(!ancestry.is_empty());
    assert!(ancestry
        .iter()
        .all(|tag| tag.get(1) == Some(&prompt.id.to_hex())));
    assert_eq!(
        std::fs::read_to_string(directory.join("model-turns")).unwrap(),
        "1"
    );
    stop(process).await;
    std::fs::remove_dir_all(directory).unwrap();
    cancel.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}
