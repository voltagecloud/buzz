use std::sync::Arc;

use buzz_db::EventQuery;
use buzz_sdk::sharing::{profile_owner, Grant};
use nostr::{Event, Timestamp};

use super::ingest::IngestError;
use crate::state::AppState;
use buzz_core::tenant::TenantContext;

pub(super) async fn validate(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
) -> Result<Grant, IngestError> {
    let reject = |text: &str| IngestError::Rejected(format!("invalid agent grant: {text}"));
    let grant = Grant::parse(event).map_err(|_| reject("malformed signed grant"))?;
    let profiles = state
        .db
        .query_events(&EventQuery {
            kinds: Some(vec![0]),
            authors: Some(vec![grant.agent.to_bytes().to_vec()]),
            limit: Some(1),
            global_only: true,
            ..EventQuery::for_community(tenant.community())
        })
        .await
        .map_err(|e| IngestError::Internal(format!("agent profile lookup failed: {e}")))?;
    let profile = profiles
        .first()
        .ok_or_else(|| reject("agent has no current profile"))?;
    if profile.event.pubkey != grant.agent
        || profile_owner(&profile.event, Timestamp::now().as_secs()).ok() != Some(event.pubkey)
    {
        return Err(reject("only the agent's verified owner may change sharing"));
    }
    if grant.enabled {
        // Explicit membership is required even for open channels. A human must
        // deliberately add their agent before exposing its configured tools.
        for member in [event.pubkey, grant.agent] {
            let is_member = state
                .db
                .is_member(tenant.community(), grant.channel, &member.to_bytes())
                .await
                .map_err(|e| {
                    IngestError::Internal(format!("grant membership lookup failed: {e}"))
                })?;
            if !is_member {
                return Err(reject("owner and agent must be channel members"));
            }
        }
        let channel = state
            .db
            .get_channel_for_event_write(tenant.community(), grant.channel)
            .await
            .map_err(|e| IngestError::Internal(format!("grant channel lookup failed: {e}")))?;
        if channel.channel_type == "dm" {
            return Err(reject("direct messages cannot be shared"));
        }
    }
    Ok(grant)
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use buzz_core::{
        channel::{ChannelType, ChannelVisibility, MemberRole},
        CommunityId,
    };
    use nostr::{EventBuilder, Keys, Kind};
    use uuid::Uuid;

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn channel_grant_requires_live_owner_and_explicit_membership() {
        let pool = sqlx::PgPool::connect(&crate::test_support::database_url())
            .await
            .unwrap();
        let state = crate::state::tests::test_state_with_database_pool(pool.clone()).await;
        let id = Uuid::new_v4();
        let host = format!("grant-{id}.test");
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(&host)
            .execute(&pool)
            .await
            .unwrap();
        let tenant = TenantContext::resolved(CommunityId::from_uuid(id), host);
        let owner = Keys::generate();
        let agent = Keys::generate();
        let stranger = Keys::generate();
        let channel = state
            .db
            .create_channel(
                tenant.community(),
                "sharing",
                ChannelType::Stream,
                ChannelVisibility::Private,
                None,
                &owner.public_key().to_bytes(),
                None,
            )
            .await
            .unwrap();
        let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
        let profile = EventBuilder::new(Kind::Metadata, "{}")
            .tag(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap())
            .sign_with_keys(&agent)
            .unwrap();
        state
            .db
            .insert_event(tenant.community(), &profile, None)
            .await
            .unwrap();
        let grant = |signer: &Keys, enabled| {
            Grant::builder(agent.public_key(), channel.id, enabled)
                .unwrap()
                .sign_with_keys(signer)
                .unwrap()
        };
        // Open-channel write permission alone cannot expose an agent's tools.
        assert!(validate(&tenant, &state, &grant(&owner, true))
            .await
            .is_err());
        // create_channel already made the human an owner. Add the agent
        // through that owner, preserving the canonical membership roles.
        state
            .db
            .add_member(
                tenant.community(),
                channel.id,
                &agent.public_key().to_bytes(),
                MemberRole::Bot,
                Some(&owner.public_key().to_bytes()),
            )
            .await
            .unwrap();
        assert!(validate(&tenant, &state, &grant(&owner, true))
            .await
            .is_ok());
        assert!(validate(&tenant, &state, &grant(&stranger, true))
            .await
            .is_err());
        assert!(validate(&tenant, &state, &grant(&stranger, false))
            .await
            .is_err());
        assert!(validate(&tenant, &state, &grant(&owner, false))
            .await
            .is_ok());
        // Bind the full ingestion seam: a removed owner can still durably
        // revoke, including after archive, but cannot re-enable sharing.
        state
            .db
            .add_member(
                tenant.community(),
                channel.id,
                &stranger.public_key().to_bytes(),
                MemberRole::Owner,
                Some(&owner.public_key().to_bytes()),
            )
            .await
            .unwrap();
        state
            .db
            .remove_member(
                tenant.community(),
                channel.id,
                &owner.public_key().to_bytes(),
                &stranger.public_key().to_bytes(),
            )
            .await
            .unwrap();
        let ingest = |event| {
            super::super::ingest::ingest_event(
                &state,
                &tenant,
                event,
                super::super::ingest::IngestAuth::Http {
                    pubkey: owner.public_key(),
                    scopes: vec![buzz_auth::Scope::MessagesWrite],
                    auth_method: super::super::ingest::HttpAuthMethod::Nip98,
                },
            )
        };
        assert!(ingest(grant(&owner, true)).await.is_err());
        assert!(ingest(grant(&owner, false)).await.unwrap().accepted);
        state
            .db
            .archive_channel(tenant.community(), channel.id)
            .await
            .unwrap();
        let revoke = Grant::builder(agent.public_key(), channel.id, false)
            .unwrap()
            .custom_created_at(Timestamp::from(Timestamp::now().as_secs() + 1))
            .sign_with_keys(&owner)
            .unwrap();
        assert!(ingest(revoke).await.unwrap().accepted);
        // A profile rotation invalidates the old owner's authority.
        let auth = buzz_sdk::nip_oa::compute_auth_tag(&stranger, &agent.public_key(), "").unwrap();
        let rotated = EventBuilder::new(Kind::Metadata, "{}")
            .custom_created_at(Timestamp::from(profile.created_at.as_secs() + 1))
            .tag(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap())
            .sign_with_keys(&agent)
            .unwrap();
        state
            .db
            .insert_event(tenant.community(), &rotated, None)
            .await
            .unwrap();
        assert!(validate(&tenant, &state, &grant(&owner, true))
            .await
            .is_err());
        assert!(validate(&tenant, &state, &grant(&owner, false))
            .await
            .is_err());
    }
}
