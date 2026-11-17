//! Atomic owner/team scope checks and bounded team status replacement.
use buzz_core::{team_status, CommunityId, StoredEvent};
use nostr::Event;
use sqlx::{Postgres, QueryBuilder};

use crate::replaceable::{ParameterizedReplacePrecondition, ParameterizedReplaceStatus};
use crate::{Db, DbError, EventQuery, Result};

impl Db {
    /// Validate authority and bounds under the publication lock, then replace the head.
    pub async fn replace_team_status(
        &self,
        community: CommunityId,
        event: &Event,
    ) -> Result<(StoredEvent, bool)> {
        let invalid = |s: &str| DbError::AccessDenied(s.into());
        let envelope = team_status::parse(event, community).map_err(invalid)?;
        team_status::validate_time(event, &envelope, chrono::Utc::now().timestamp() as u64)
            .map_err(invalid)?;
        let mut tx = crate::begin_community_event_write_transaction_with_legacy_metrics(
            &self.pool,
            community,
            crate::observability::WriterOperation::EventWrite,
        )
        .await?;
        // No push enqueue: the subject tag is not a mention.
        let owner = event.pubkey.to_bytes();
        let agent = envelope.agent.to_bytes();
        let coordinate = format!(
            "team-status:{community}:{}:{}:",
            envelope.agent.to_hex(),
            envelope.channel
        );
        let lock = super::replaceable::event_replacement_lock_key(
            community,
            30623,
            &owner,
            Some(coordinate.as_bytes()),
        );
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(lock)
            .execute(tx.conn())
            .await?;
        team_status::validate_time(event, &envelope, chrono::Utc::now().timestamp() as u64)
            .map_err(invalid)?;
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE community_id=$1 AND pubkey=$2 AND agent_owner_pubkey=$3)")
            .bind(community.as_uuid()).bind(agent.as_slice()).bind(owner.as_slice()).fetch_one(tx.conn()).await?;
        if !owned {
            return Err(invalid(
                "only the actual agent owner may publish team status",
            ));
        }
        // Row locks fence concurrent channel deletion and membership removal.
        let channel: Option<String> = sqlx::query_scalar("SELECT channel_type::text FROM channels WHERE community_id=$1 AND id=$2 AND deleted_at IS NULL AND archived_at IS NULL FOR SHARE")
            .bind(community.as_uuid()).bind(envelope.channel).fetch_optional(tx.conn()).await?;
        if !matches!(channel.as_deref(), Some("stream" | "forum")) {
            return Err(invalid(
                "team status requires a live stream or forum channel",
            ));
        }
        let members: Vec<Vec<u8>> = sqlx::query_scalar("SELECT pubkey FROM channel_members WHERE community_id=$1 AND channel_id=$2 AND pubkey=ANY($3) AND removed_at IS NULL ORDER BY pubkey FOR SHARE")
            .bind(community.as_uuid()).bind(envelope.channel).bind(vec![owner.to_vec(), agent.to_vec()]).fetch_all(tx.conn()).await?;
        if !members.contains(&owner.to_vec()) || !members.contains(&agent.to_vec()) {
            return Err(invalid("owner and agent must be current channel members"));
        }
        let root: Option<Vec<u8>> = sqlx::query_scalar("SELECT id FROM events WHERE community_id=$1 AND id=$2 AND channel_id=$3 AND deleted_at IS NULL AND kind IN (9,40002,40008,45001) AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(tags) t WHERE t->>0='e' AND (t->>3 IN ('root','reply') OR jsonb_array_length(t)<4)) FOR SHARE")
            .bind(community.as_uuid()).bind(envelope.root.as_bytes().as_slice()).bind(envelope.channel).fetch_optional(tx.conn()).await?;
        if root.is_none() {
            return Err(invalid(
                "team status must reference a live thread root in its channel",
            ));
        }
        let (recent, active, stored): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE received_at > now()-interval '1 minute'), \
             count(*) FILTER (WHERE deleted_at IS NULL AND content<>'' AND EXISTS(SELECT 1 FROM jsonb_array_elements(tags) t WHERE t->>0='expiration' AND (t->>1)::numeric > extract(epoch FROM now())) AND d_tag<>$4), \
             count(DISTINCT d_tag) FILTER (WHERE d_tag<>$4) \
             FROM events WHERE community_id=$1 AND kind=30623 AND pubkey=$2 AND starts_with(d_tag,$3)")
            .bind(community.as_uuid()).bind(owner.as_slice()).bind(&coordinate).bind(&envelope.address).fetch_one(tx.conn()).await?;
        if recent >= team_status::MAX_WRITES_PER_MINUTE {
            return Err(invalid("team status publication rate exceeded"));
        }
        if !matches!(envelope.content, team_status::Content::Clear)
            && active >= team_status::MAX_ACTIVE_THREADS
        {
            return Err(invalid("too many active team status threads"));
        }
        // Expiry is not erasure. Bound retained addresses independently of active cards.
        if stored >= 1024 {
            return Err(invalid("team status retained address quota exceeded"));
        }
        let result = self
            .replace_parameterized_event_in_transaction(
                &mut tx,
                event,
                &envelope.address,
                Some(envelope.channel),
                ParameterizedReplacePrecondition::Unconditional,
            )
            .await?;
        let inserted = result.status == ParameterizedReplaceStatus::Inserted;
        tx.commit().await?;
        Ok((result.event, inserted))
    }
}

/// Apply audience and root checks before LIMIT/COUNT. Unscoped internal reads hide statuses.
pub(crate) fn push_visibility(qb: &mut QueryBuilder<Postgres>, prefix: &str, q: &EventQuery) {
    let table = if prefix.is_empty() { "events" } else { "e" };
    let explicit = q.ids.is_some() || q.kinds.as_ref().is_some_and(|k| k.contains(&30623));
    let Some(reader) = q.team_status_reader.as_ref().filter(|_| explicit) else {
        qb.push(format!(" AND {table}.kind <> 30623"));
        return;
    };
    qb.push(format!(" AND ({table}.kind <> 30623 OR ("));
    qb.push(format!("{table}.tags @> jsonb_build_array(jsonb_build_array('community', {table}.community_id::text)) "));
    qb.push(format!("AND EXISTS(SELECT 1 FROM channels c WHERE c.community_id={table}.community_id AND c.id={table}.channel_id AND c.deleted_at IS NULL AND c.archived_at IS NULL AND c.channel_type IN ('stream','forum') AND (c.visibility <> 'private' OR EXISTS(SELECT 1 FROM channel_members cm WHERE cm.community_id=c.community_id AND cm.channel_id=c.id AND cm.removed_at IS NULL AND cm.pubkey="));
    qb.push_bind(reader.clone());
    qb.push("))) ");
    qb.push(format!("AND EXISTS(SELECT 1 FROM events root WHERE root.community_id={table}.community_id AND root.channel_id={table}.channel_id AND root.deleted_at IS NULL AND root.kind IN (9,40002,40008,45001) AND root.id=decode((SELECT t->>1 FROM jsonb_array_elements({table}.tags) t WHERE t->>0='e' AND t->>3='root' LIMIT 1),'hex'))"));
    qb.push(format!(" AND EXISTS(SELECT 1 FROM channel_members owner_member WHERE owner_member.community_id={table}.community_id AND owner_member.channel_id={table}.channel_id AND owner_member.pubkey={table}.pubkey AND owner_member.removed_at IS NULL)"));
    qb.push(format!(" AND EXISTS(SELECT 1 FROM channel_members agent_member JOIN users agent ON agent.community_id=agent_member.community_id AND agent.pubkey=agent_member.pubkey WHERE agent_member.community_id={table}.community_id AND agent_member.channel_id={table}.channel_id AND agent_member.removed_at IS NULL AND agent.agent_owner_pubkey={table}.pubkey AND {table}.tags @> jsonb_build_array(jsonb_build_array('p',encode(agent_member.pubkey,'hex'))))"));
    qb.push("))");
}

/// Status reads make current authorization decisions and cannot use a lagging replica.
pub(crate) fn requires_writer(q: &EventQuery) -> bool {
    q.team_status_reader.is_some()
        && (q.ids.is_some() || q.kinds.as_ref().is_some_and(|k| k.contains(&30623)))
}
