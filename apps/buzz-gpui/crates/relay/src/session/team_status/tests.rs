use super::*;

fn preview(p: &Publisher, root: &str) -> Preview {
    Preview {
        scope: Scope {
            community: "11111111-1111-4111-8111-111111111111".into(),
            agent: "aa".repeat(32),
            channel: "22222222-2222-4222-8222-222222222222".into(),
            root: root.into(),
        },
        label: label(Status::Working, "DEV-123 public task").unwrap(),
        generation: p.generation.load(Ordering::SeqCst),
        origin: "https://origin".into(),
        owner: "owner".into(),
    }
}

#[test]
fn durable_owner_journey_uncertain_publish_stop_restart_and_expired_clear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.sqlite");
    let store = Store::open(&path).unwrap();
    let publisher = Publisher::new(store.clone(), "owner".into());
    assert_eq!(publisher.state().sharing, 0);
    let approved = preview(&publisher, &"bb".repeat(32));
    assert!(publisher.approve(approved.clone(), false).is_err());
    publisher.approve(approved.clone(), true).unwrap();
    let sending = publisher.next("https://origin", 100).unwrap().unwrap();
    let signed = sign(
        &buzz_identity::Identity::ephemeral(),
        template(&sending).unwrap(),
    )
    .unwrap();
    assert_eq!(signed.tags.len(), 6); // Outbox's client-id/ms tags must never appear.
    assert_eq!(signed.kind.as_u16(), KIND);
    assert_eq!(signed.created_at.as_secs(), 100);
    assert!(
        signed
            .tags
            .iter()
            .any(|t| t.as_slice() == ["expiration", "1000"])
    );
    assert!(signed.content.contains("working: DEV-123 public task"));
    publisher.stop().unwrap(); // While the label's HTTP request is still in flight.
    publisher.finish(&sending, Ok(())).unwrap();
    assert_eq!(publisher.state().sharing, 0);
    assert_eq!(publisher.state().pending, 1);
    assert!(publisher.approve(approved, true).is_err()); // stale preview fenced
    let clear = publisher.next("https://origin", 100).unwrap().unwrap();
    assert_eq!(clear.last, 101);
    assert!(template(&clear).unwrap().content.is_empty());
    publisher.finish(&clear, Err("offline".into())).unwrap();
    assert_eq!(publisher.state().error.as_deref(), Some("offline"));
    drop(publisher);
    drop(store);
    let recovered = Publisher::new(Store::open(&path).unwrap(), "owner".into());
    let newer = recovered.next("https://origin", 2000).unwrap().unwrap(); // Previous signed clear expired.
    assert_eq!(newer.last, 2000);
    let event = template(&newer).unwrap();
    assert!(event.content.is_empty());
    assert!(
        event
            .tags
            .iter()
            .any(|t| t.as_slice() == ["expiration", "2900"])
    );
    recovered.finish(&newer, Ok(())).unwrap();
    assert_eq!(recovered.state().pending, 0);
    // Explicit consent can enable again after a clear, without reusing old consent.
    recovered
        .approve(preview(&recovered, &"bb".repeat(32)), true)
        .unwrap();
    drop(recovered);
    let recovered = Publisher::new(Store::open(&path).unwrap(), "owner".into());
    assert_eq!(recovered.state().sharing, 0); // Restart fails closed.
    assert!(
        recovered
            .next("https://origin", 2001)
            .unwrap()
            .unwrap()
            .desired
            .is_none()
    );
}

#[test]
fn retry_rotation_scope_capacity_and_clock_bounds() {
    let p = Publisher::new(Store::in_memory().unwrap(), "owner".into());
    for i in 0..CAPACITY {
        p.approve(preview(&p, &format!("{i:064x}")), true).unwrap();
    }
    assert!(p.approve(preview(&p, &"ff".repeat(32)), true).is_err());
    p.stop().unwrap();
    let first = p.next("https://origin", 100).unwrap().unwrap();
    p.finish(&first, Err("root unavailable".into())).unwrap();
    let second = p.next("https://origin", 100).unwrap().unwrap();
    assert_ne!(first.scope.root, second.scope.root); // Failure can't starve another clear.
    assert_eq!(p.state().pending, CAPACITY);
    assert!(p.next("https://origin", 0).is_ok()); // Other addresses remain eligible.
}

#[test]
fn caption_contract_and_exact_six_tags() {
    for caption in [
        "@someone",
        "a/b",
        "a\\b",
        "`cmd`",
        "https://x",
        "nostr:abc",
        "line\nline",
        "line\u{2028}line",
        " padded ",
    ] {
        assert!(label(Status::Working, caption).is_err(), "{caption:?}");
    }
    for status in [
        Status::Working,
        Status::Waiting,
        Status::Blocked,
        Status::Done,
    ] {
        assert_eq!(label(status, "").unwrap(), status.label());
    }
    let p = Publisher::new(Store::in_memory().unwrap(), "owner".into());
    p.approve(preview(&p, &"bb".repeat(32)), true).unwrap();
    let e = p.next("https://origin", 100).unwrap().unwrap();
    assert_eq!(template(&e).unwrap().tags.len(), 6);
    assert!(p.next("https://origin", 0).is_err()); // No unbounded future timestamps after rollback.
}

#[test]
fn one_owner_publisher_fences_all_communities_and_rejects_foreign_previews() {
    let store = Store::in_memory().unwrap();
    let owner = format!("owner-{}", uuid::Uuid::new_v4());
    let a = Publisher::shared(store.clone(), owner.clone());
    let b = Publisher::shared(store, owner.clone());
    assert!(Arc::ptr_eq(&a, &b));
    let mut first = preview(&a, &"bb".repeat(32));
    assert!(a.approve(first.clone(), true).is_err());
    first.owner = owner.clone();
    a.approve(first, true).unwrap();
    let mut second = preview(&b, &"cc".repeat(32));
    second.owner = owner;
    second.origin = "https://other-origin".into();
    second.scope.community = "33333333-3333-4333-8333-333333333333".into();
    b.approve(second.clone(), true).unwrap();
    a.stop().unwrap();
    assert_eq!(b.state().sharing, 0);
    assert_eq!(b.state().pending, 2);
    assert!(b.approve(second, true).is_err());
    assert!(a.next("https://unrelated", 100).unwrap().is_none());
    assert!(
        a.next("https://other-origin", 100)
            .unwrap()
            .unwrap()
            .desired
            .is_none()
    );
}
