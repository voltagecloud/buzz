//! The production polling service must retry authority failures before reservation.
use super::*;

#[tokio::test]
async fn preclaim_authority_failure_does_not_consume_a_still_live_approval() {
    let mut fixture = Fixture::new(Mode::TransientPreclaim).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.config.relay = format!("ws://{}", listener.local_addr().unwrap());
    let shared = fixture.state.clone();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            json!(["AUTH", "snapshot-service-fixture"])
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value[0] != "EVENT" && value[0] != "AUTH" {
                continue;
            }
            let event: Event = serde_json::from_value(value[1].clone()).unwrap();
            assert!(event.verify().is_ok());
            let mut message = "saved";
            if value[0] == "EVENT" {
                if phase(&event, ":accepted") {
                    message = attach::CLAIM_OK_MESSAGE;
                }
                if phase(&event, ":upload") {
                    assert_saved_intent(&shared, &event);
                }
                shared.lock().unwrap().events.push(event.clone());
            }
            ws.send(Message::Text(
                json!(["OK", event.id, true, message]).to_string().into(),
            ))
            .await
            .unwrap();
        }
    });
    let config = fixture.config.clone();
    let rest = fixture.rest.clone();
    let (shutdown, rx) = watch::channel(());
    let service = tokio::spawn(async move {
        run(&config, &rest, PathBuf::from("/unused-fixture-helper"), rx).await
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if fixture.state.lock().unwrap().authority_failures > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture
        .state
        .lock()
        .unwrap()
        .events
        .iter()
        .any(|event| phase(event, ":accepted")));
    assert_eq!(
        std::fs::read_dir(&fixture.config.journal)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|value| value == "json"))
            .count(),
        0,
        "failed preclaim authority consumed an approval without a claim attempt"
    );
    fixture.state.lock().unwrap().deny_authority = false;
    timeout(Duration::from_secs(15), async {
        loop {
            if fixture
                .state
                .lock()
                .unwrap()
                .events
                .iter()
                .any(|event| phase(event, ":ready"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.state.lock().unwrap().uploads, 1);
    shutdown.send(()).unwrap();
    timeout(Duration::from_secs(40), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.abort();
}

#[tokio::test]
async fn unconfirmed_snapshot_containment_propagates_without_end_or_cleanup() {
    let mut fixture = Fixture::new(Mode::Success).await;
    let original = std::fs::read_to_string(&fixture.config.executable).unwrap();
    std::fs::write(&fixture.config.executable, original + "\nsys.exit(2)\n").unwrap();
    let journal = Journal::open(
        fixture.config.journal.clone(),
        fixture.config.scope,
        fixture.config.owner,
    )
    .unwrap();
    let mut record = journal
        .reserve(&fixture.config.keys, &fixture.grant)
        .unwrap();
    record.claimed = true;
    journal.save(&record).unwrap();
    let (_shutdown, rx) = watch::channel(());
    let error = export(
        &fixture.config,
        &fixture.rest,
        &mut fixture.connection,
        &journal,
        &fixture.grant,
        &mut record,
        PathBuf::from("/unused-fixture-helper"),
        rx,
    )
    .await
    .unwrap_err();
    assert!(error.is::<client::ContainmentError>());
    assert!(record.ended.is_none());
    assert!(
        journal.job(fixture.grant.id()).exists(),
        "unconfirmed containment removed its durable cleanup boundary"
    );
    assert_eq!(fixture.state.lock().unwrap().uploads, 0);
}
