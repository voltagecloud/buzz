use super::*;

#[tokio::test]
async fn deleted_retyped_and_ambiguous_listings_do_not_start_saved_registrations() {
    for runtime in [None, Some("local/agent"), Some("ambiguous")] {
        let relay = relay::Relay::start().await;
        let cluster = sandbox_fake::Shared::default();
        let identity = register(&cluster, &relay.url).await;
        let trigger = message("@agent use the current runtime", None);
        {
            let mut events = relay.events.lock().unwrap();
            events.retain(|event| event.kind.as_u16() != 30177);
            if let Some(runtime) = runtime {
                let mut tags = vec![
                    Tag::parse(["d", &keys("22").public_key().to_hex()]).unwrap(),
                    Tag::parse(["runtime", runtime]).unwrap(),
                ];
                if runtime == "ambiguous" {
                    tags.push(Tag::parse(["runtime", "voltage.remote-workspace/sandbox"]).unwrap());
                }
                events.push(
                    EventBuilder::new(Kind::Custom(30177), "{}")
                        .tags(tags)
                        .sign_with_keys(&keys("33"))
                        .unwrap(),
                );
            }
            events.push(trigger.clone());
        }
        let mut journal = Journal::acquire(sandbox_fake::client(&cluster), &identity, "worker", 0)
            .await
            .unwrap();
        // A trigger already persisted before revocation must be rechecked too.
        journal.enqueue(trigger.clone()).await.unwrap();
        let task = {
            let manager = manager(&cluster);
            tokio::spawn(async move {
                worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
            })
        };
        wait_for(|| completed(&cluster, &trigger)).await;
        assert!(
            cluster.lock().unwrap().sandboxes.is_empty(),
            "stale runtime {runtime:?} launched"
        );
        assert!(inbox(&cluster)["outbox"].as_array().unwrap().is_empty());
        task.abort();
        let _ = task.await;
        relay.stop().await;
    }
}
