//! Postgres-backed ingest tests for NIP-IG route records: kind:30624 records
//! at `route:<source>:<key>` keep the greatest route, not the newest write.

use std::sync::Arc;

use buzz_core::{kind::KIND_INTEGRATION_RECORD, TenantContext};
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use uuid::Uuid;

use crate::handlers::group_state_postgres_tests::{community, create_channel, http_auth, state};
use crate::handlers::ingest::{ingest_event, IngestError};
use crate::state::AppState;

fn record(keys: &Keys, channel: Uuid, d: &str, tags: &[&[&str]], at: u64) -> Event {
    let mut all = vec![
        Tag::parse(["d", d]).unwrap(),
        Tag::parse(["h", &channel.to_string()]).unwrap(),
    ];
    all.extend(
        tags.iter()
            .map(|tag| Tag::parse(tag.iter().copied()).unwrap()),
    );
    EventBuilder::new(Kind::Custom(KIND_INTEGRATION_RECORD as u16), "")
        .tags(all)
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .expect("sign record")
}

/// `Ok(true)` when stored, `Ok(false)` when acknowledged as a duplicate.
async fn submit(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    keys: &Keys,
    event: Event,
) -> Result<bool, IngestError> {
    let result = ingest_event(state, tenant, event, http_auth(keys)).await?;
    assert!(result.accepted);
    Ok(!result.message.starts_with("duplicate:"))
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn route_records_keep_the_greatest_route_and_refuse_unreadable_ones() {
    let state = state().await;
    let tenant = community(&state, "integration-route").await;
    let publisher = Keys::generate();
    let channel = create_channel(&state, &tenant, &publisher, &[]).await;
    let route = format!("route:linear:{}", "ab".repeat(32));
    let now = Timestamp::now().as_secs();
    let (low, high) = (
        Uuid::from_u128(1).to_string(),
        Uuid::from_u128(2).to_string(),
    );

    // A route record without a readable route is refused before storage.
    for tags in [
        &[][..],
        &[&["updated", "soon"][..]][..],
        &[&["updated", "1"][..], &["updated", "2"][..]][..],
        &[&["updated", "1"][..], &["route", "general"][..]][..],
    ] {
        match submit(
            &state,
            &tenant,
            &publisher,
            record(&publisher, channel, &route, tags, now),
        )
        .await
        {
            Err(IngestError::Rejected(message)) => assert!(message.starts_with("invalid: ")),
            other => panic!("{tags:?} must be refused, got {other:?}"),
        }
    }

    // The publisher learns that its write lost from the duplicate reply.
    let steps = [
        ("5", Some(&low), now, true),
        // An older version written later, as a late write after a restart.
        ("4", Some(&high), now + 30, false),
        // A newer clear written earlier.
        ("6", None, now - 30, true),
        // The same version mapped: a channel outranks a clear.
        ("6", Some(&low), now - 31, true),
    ];
    for (index, (version, routed, at, stored)) in steps.into_iter().enumerate() {
        let mut tags = vec![["updated", version]];
        tags.extend(routed.map(|channel| ["route", channel.as_str()]));
        let tags: Vec<&[&str]> = tags.iter().map(|tag| &tag[..]).collect();
        let event = record(&publisher, channel, &route, &tags, at);
        assert_eq!(
            submit(&state, &tenant, &publisher, event).await.unwrap(),
            stored,
            "step {index}"
        );
    }

    // Earlier releases' route plans keep write-time replacement.
    let released = "route:linear:issue:ENG-1";
    for (at, stored) in [(now, true), (now + 1, true), (now - 1, false)] {
        let event = record(&publisher, channel, released, &[], at);
        assert_eq!(
            submit(&state, &tenant, &publisher, event).await.unwrap(),
            stored,
            "released at {at}"
        );
    }
}
