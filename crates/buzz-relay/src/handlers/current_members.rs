//! Current channel authority for scoped strong Nostr roster reads.
//!
//! A stored discovery event can lag canonical membership after projection
//! publication fails. Reconcile that one coordinate behind the existing writer
//! lock before returning authority. Ordinary and historical queries retain
//! their existing behavior. This path never fans out events.

use std::sync::Arc;

use buzz_core::{tenant::TenantContext, StoredEvent};
use buzz_db::{DbError, EventQuery};
use nostr::{Alphabet, Filter, SingleLetterTag};

use crate::state::AppState;

/// Return current membership for an explicit strong authority filter, or `None`
/// when this is an ordinary event-history filter. One filter repairs at most
/// one channel; existing request filter/concurrency limits bound total work.
pub(crate) async fn read(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    reader: &[u8],
    filter: &Filter,
) -> Result<Option<Vec<StoredEvent>>, DbError> {
    if filter.kinds.as_ref().is_none_or(|kinds| {
        kinds.len() != 1
            || !kinds
                .iter()
                .any(|kind| u32::from(kind.as_u16()) == buzz_core::kind::KIND_NIP29_GROUP_MEMBERS)
    }) || filter.authors.as_ref().is_none_or(|authors| {
        authors.len() != 1 || !authors.contains(&state.relay_keypair.public_key())
    }) || filter.ids.is_some()
        || filter.since.is_some()
        || filter.until.is_some()
        || filter.search.is_some()
        || filter.generic_tags.len() != 1
    {
        return Ok(None);
    }
    let d = SingleLetterTag::lowercase(Alphabet::D);
    let Some(values) = filter
        .generic_tags
        .get(&d)
        .filter(|values| values.len() == 1)
    else {
        return Ok(None);
    };
    let Some(value) = values.iter().next() else {
        return Ok(None);
    };
    let Ok(channel) = uuid::Uuid::parse_str(value) else {
        return Ok(Some(Vec::new()));
    };
    if channel.to_string() != *value || filter.limit == Some(0) {
        return Ok(Some(Vec::new()));
    }

    // Read before taking the lock: never nest a pool checkout while holding
    // a membership transaction. The locked canonical tags below decide whether
    // this head remains authoritative, regardless of intervening publication.
    let existing = state
        .db
        .query_events(&EventQuery {
            kinds: Some(vec![buzz_core::kind::KIND_NIP29_GROUP_MEMBERS as i32]),
            authors: Some(vec![state.relay_keypair.public_key().to_bytes().to_vec()]),
            channel_id: Some(channel),
            limit: Some(1),
            ..EventQuery::for_community(tenant.community())
        })
        .await?
        .into_iter()
        .next();
    let relay = state.relay_keypair.public_key().to_bytes();
    let mut snapshot = state
        .db
        .lock_member_snapshot(tenant.community(), channel, &relay)
        .await?;
    // A cached access list must not let a removed reader repair or read a
    // private channel's current membership. No writes precede this check.
    if snapshot.identity.is_none()
        || !snapshot
            .members
            .iter()
            .any(|member| member.pubkey == reader)
    {
        snapshot.release().await?;
        return Ok(Some(Vec::new()));
    }
    let expected = super::side_effects::group_members_tags(
        &channel.to_string(),
        snapshot.identity.as_ref(),
        &snapshot.members,
    )
    .map_err(|_| DbError::InvalidData("could not build channel roster".into()))?;
    let tags = |tags: &nostr::Tags| {
        let mut values = tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        values.sort();
        values
    };
    let mut expected = expected
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect::<Vec<_>>();
    expected.sort();
    if let Some(existing) = existing
        .filter(|stored| stored.event.verify().is_ok() && tags(&stored.event.tags) == expected)
    {
        snapshot.release().await?;
        return Ok(Some(vec![existing]));
    }
    let repaired = super::side_effects::store_group_members_event(state, &mut snapshot)
        .await
        .map_err(|_| DbError::InvalidData("could not repair channel roster".into()))?;
    snapshot.release().await?;
    Ok(Some(repaired.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;
    use serde_json::json;

    /// Simulate committed membership removal with failed discovery publication.
    /// Strong roster reads repair it from canonical rows; a removed reader
    /// cannot repair or observe current private state. Repeated reads do not
    /// create replacement churn, and database failure propagates.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn strong_roster_recovers_failed_projection_without_leaking_or_churning() {
        let state = crate::state::tests::test_state().await;
        let host = format!("gateway-roster-{}.test", uuid::Uuid::new_v4());
        let community = state
            .db
            .ensure_configured_community(&host)
            .await
            .unwrap()
            .id;
        let tenant = TenantContext::resolved(community, host);
        let (owner, agent, gateway, stranger) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
        );
        let channel = uuid::Uuid::new_v4();
        state
            .db
            .create_channel_with_id(
                community,
                channel,
                "private-gateway-roster",
                buzz_db::channel::ChannelType::Stream,
                buzz_db::channel::ChannelVisibility::Private,
                None,
                owner.public_key().as_bytes(),
                None,
            )
            .await
            .unwrap();
        for key in [&agent, &gateway] {
            state
                .db
                .add_member(
                    community,
                    channel,
                    key.public_key().as_bytes(),
                    buzz_core::channel::MemberRole::Member,
                    Some(owner.public_key().as_bytes()),
                )
                .await
                .unwrap();
        }
        let relay = state.relay_keypair.public_key();
        let filter: Filter = serde_json::from_value(
            json!({"kinds":[39002], "authors":[relay], "#d":[channel], "limit":1}),
        )
        .unwrap();
        let stored_query = EventQuery {
            kinds: Some(vec![39002]),
            authors: Some(vec![relay.to_bytes().to_vec()]),
            channel_id: Some(channel),
            limit: Some(1),
            ..EventQuery::for_community(community)
        };
        // Extra generic constraints retain ordinary filter semantics and must
        // never be answered as an unconstrained current authority query.
        let constrained: Filter = serde_json::from_value(json!({
            "kinds":[39002], "authors":[relay], "#d":[channel],
            "#p":[stranger.public_key()], "limit":1
        }))
        .unwrap();
        assert!(
            read(&state, &tenant, agent.public_key().as_bytes(), &constrained)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .db
                .query_events(&stored_query)
                .await
                .unwrap()
                .is_empty(),
            "an ordinary constrained query must not repair the projection"
        );
        let original = read(&state, &tenant, agent.public_key().as_bytes(), &filter)
            .await
            .unwrap()
            .unwrap()
            .pop()
            .unwrap();
        assert!(buzz_sdk::gateway::channel_member(
            &original.event,
            relay,
            channel,
            gateway.public_key()
        ));
        // Canonical membership changes without emitting discovery metadata.
        state
            .db
            .remove_member(
                community,
                channel,
                gateway.public_key().as_bytes(),
                owner.public_key().as_bytes(),
            )
            .await
            .unwrap();
        let stale = state
            .db
            .query_events(&stored_query)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(stale.event.id, original.event.id);
        assert!(buzz_sdk::gateway::channel_member(
            &stale.event,
            relay,
            channel,
            gateway.public_key()
        ));
        for reader in [&gateway, &stranger] {
            assert!(
                read(&state, &tenant, reader.public_key().as_bytes(), &filter)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            state.db.query_events(&stored_query).await.unwrap()[0]
                .event
                .id,
            original.event.id,
            "unauthorized reads did not repair or publish"
        );
        let current = read(&state, &tenant, agent.public_key().as_bytes(), &filter)
            .await
            .unwrap()
            .unwrap()
            .pop()
            .unwrap();
        assert!(!buzz_sdk::gateway::channel_member(
            &current.event,
            relay,
            channel,
            gateway.public_key()
        ));
        assert_ne!(current.event.id, original.event.id);
        let unchanged = read(&state, &tenant, agent.public_key().as_bytes(), &filter)
            .await
            .unwrap()
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            unchanged.event.id, current.event.id,
            "current state does not churn"
        );
        let foreign: Filter = serde_json::from_value(
            json!({"kinds":[39002], "authors":[stranger.public_key()], "#d":[channel]}),
        )
        .unwrap();
        assert!(
            read(&state, &tenant, stranger.public_key().as_bytes(), &foreign)
                .await
                .unwrap()
                .is_none()
        );
        state.db.pool().close().await;
        assert!(
            read(&state, &tenant, agent.public_key().as_bytes(), &filter)
                .await
                .is_err()
        );
    }
}
