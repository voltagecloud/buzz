use super::*;

fn workflow(content: &str, mentioned: nostr::PublicKey) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
            Tag::parse(["buzz:workflow", "true"]).unwrap(),
            Tag::parse([buzz_sdk::workflow::SCHEDULED_LITERAL_TAG, "literal-v1"]).unwrap(),
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
    let unproven = EventBuilder::new(forged.kind, "@agent unproven workflow")
        .tags(
            workflow("unused", keys("22").public_key())
                .tags
                .iter()
                .filter(|tag| {
                    tag.as_slice().first().map(String::as_str)
                        != Some(buzz_sdk::workflow::SCHEDULED_LITERAL_TAG)
                })
                .cloned(),
        )
        .sign_with_keys(&keys("11"))
        .unwrap();
    assert!(
        workflow_sender
            .send_event(unproven.clone())
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
    assert!(
        !completed(&cluster, &unproven),
        "unproven workflow was admitted"
    );
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);

    let now = Timestamp::now().as_secs();
    let expired = buzz_sdk::gateway::Delegation::grant(keys("44").public_key(), now - 1, now - 10)
        .unwrap()
        .sign_with_keys(&keys("33"))
        .unwrap();
    for (state, head, member, reason) in [
        (
            "revoked",
            gateway::delegation(false, 5),
            true,
            "not currently delegated",
        ),
        ("expired", expired, true, "not currently delegated"),
        (
            "left",
            gateway::delegation(true, 5),
            false,
            "not a current posting member",
        ),
    ] {
        {
            let mut events = relay.events.lock().unwrap();
            events.retain(|event| !matches!(event.kind.as_u16(), 30181 | 39002));
            events.extend([head, gateway::roster(member)]);
        }
        let refused = workflow(
            &format!("@agent workflow after gateway {state}"),
            keys("22").public_key(),
        );
        assert!(
            workflow_sender
                .send_event(refused.clone())
                .await
                .unwrap()
                .accepted
        );
        wait_for(|| {
            relay.events.lock().unwrap().iter().any(|event| {
                event.content.contains(reason)
                    && event
                        .tags
                        .iter()
                        .any(|tag| tag.as_slice().get(1) == Some(&refused.id.to_hex()))
            })
        })
        .await;
        assert_eq!(
            cluster.lock().unwrap().sandboxes.len(),
            1,
            "{state} workflow launched"
        );
    }
    stop.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
    relay.stop().await;
}

#[tokio::test(start_paused = true)]
async fn gateway_workflow_launch_keeps_the_owners_namespace_quota_refusal() {
    use http::{Method, Request, Response};
    use kube::client::Body;
    let cluster = sandbox_fake::Shared::default();
    register(&cluster, "wss://relay.example.invalid").await;
    let inner = sandbox_fake::client(&cluster);
    let captured = cluster.clone();
    let service = tower::service_fn(move |request: Request<Body>| {
        let (inner, cluster) = (inner.clone(), captured.clone());
        async move {
            let path = request.uri().path().to_owned();
            assert!(
                path.contains(&format!("/namespaces/{NS}/")),
                "workflow escaped owner's namespace: {path}"
            );
            if request.method() == Method::GET && path.contains("/pods/") {
                return Ok::<_,kube::Error>(Response::builder().status(404).body(Body::from(r#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"pod not created","code":404}"#.as_bytes().to_vec())).unwrap());
            }
            if request.method() == Method::GET && path.contains("/sandboxes/") {
                let name = path.rsplit('/').next().unwrap();
                let sandbox = { cluster.lock().unwrap().sandboxes.get(name).cloned() };
                if let Some(mut sandbox) = sandbox {
                    sandbox["status"] = json!({"conditions":[{"type":"Ready","status":"False","reason":"ReconcilerError","message":"pods refused: exceeded quota: primary-sessions, requested: pods=1, used: pods=10, limited: pods=10"}]});
                    return Ok(Response::builder()
                        .status(200)
                        .body(Body::from(serde_json::to_vec(&sandbox).unwrap()))
                        .unwrap());
                }
            }
            let response = inner.send(request).await?;
            // The fixture controller also obeys the quota: it cannot create a Pod.
            {
                let mut state = cluster.lock().unwrap();
                state.pods.clear();
                state.pod_created.clear();
            }
            Ok(response)
        }
    });
    let config = ManagerConfig::parse(&json!({"version":1,"checkpoint_bucket":"fixture-checkpoints","developers":{"dev":{
        "owner_pubkey":keys("33").public_key(),"namespace":NS,
        "profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"
    }}}).to_string()).unwrap();
    let manager = Manager::new(keys("11"), config, kube::Client::new(service, NS), crate::enroll::tests::offline(Default::default()))
        .with_sandbox_defaults(json!({"image":format!("example.invalid/agent@sha256:{}", "a".repeat(64)),
            "pod_options":{"active_deadline_seconds":3600,"priority_class_name":"remote-agent-session-staging"}}).as_object().unwrap().clone());
    let now = Timestamp::now().as_secs();
    let head = buzz_sdk::gateway::Delegation::grant(keys("44").public_key(), now + 3600, now)
        .unwrap()
        .sign_with_keys(&keys("33"))
        .unwrap();
    let delegation =
        buzz_sdk::gateway::live(&head, keys("33").public_key(), keys("44").public_key(), now)
            .unwrap();
    let request = crate::mobile_launch::GatewayRequest {
        delegation,
        workflow_relay: Some(keys("11").public_key()),
    };
    let trigger = workflow("@agent bounded scheduled work", keys("22").public_key());
    let result = manager
        .start_mention(
            keys("33").public_key(),
            keys("22").public_key(),
            "wss://relay.example.invalid",
            &trigger,
            Some(&request),
        )
        .await;
    assert_eq!(result["ok"], false, "{result}");
    assert!(
        result.to_string().contains("agent sessions are in use"),
        "{result}"
    );
    assert!(
        cluster.lock().unwrap().pods.is_empty(),
        "quota refusal became a running Pod"
    );
    assert!(
        cluster
            .lock()
            .unwrap()
            .sandboxes
            .values()
            .any(
                |sandbox| sandbox["spec"]["podTemplate"]["spec"]["priorityClassName"]
                    == "remote-agent-session-staging"
            ),
        "workflow removed the configured primary-session quota class"
    );
}
