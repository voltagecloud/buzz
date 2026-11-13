//! Postgres-backed ingest tests for NIP-AT agent session transcripts
//! (kind 44201): the relay stores a part only when its content is NIP-44
//! ciphertext, its envelope is canonical, and its `p` recipient is the
//! agent's owner or a gateway the owner currently delegates to.

use std::sync::Arc;

use buzz_auth::Scope;
use buzz_core::kind::KIND_AGENT_SESSION_TRANSCRIPT;
use buzz_core::TenantContext;
use buzz_sdk::transcript::{
    build_agent_session_transcript, encrypt_part, TranscriptEnvelope, TranscriptPayload,
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag};
use uuid::Uuid;

use crate::handlers::group_state_postgres_tests::{community, create_channel, http_auth, state};
use crate::handlers::ingest::{ingest_event, HttpAuthMethod, IngestAuth, IngestError};
use crate::state::AppState;

fn payload(root: EventId) -> TranscriptPayload {
    TranscriptPayload {
        v: 1,
        turn_id: "turn-1".into(),
        turn_seq: 1,
        thread_root: root.to_hex(),
        started_at: "2026-11-13T10:00:00Z".into(),
        ended_at: "2026-11-13T10:01:00Z".into(),
        stop_reason: "end_turn".into(),
        usage: None,
        context: None,
        workspace: Vec::new(),
        omitted: 0,
        updates: vec![serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "done"}
        })],
    }
}

fn part(agent: &Keys, recipient: PublicKey, channel: Uuid, root: EventId) -> Event {
    let content = encrypt_part(agent, &recipient, &payload(root)).expect("encrypt");
    let envelope = TranscriptEnvelope {
        recipient,
        thread_root: root,
        channel,
        turn_id: "turn-1".into(),
        part_index: 0,
        part_count: 1,
    };
    build_agent_session_transcript(&envelope, &content)
        .expect("build")
        .sign_with_keys(agent)
        .expect("sign")
}

/// The same event shape with arbitrary content and tags.
fn raw(agent: &Keys, content: &str, tags: Vec<Vec<String>>) -> Event {
    EventBuilder::new(
        Kind::Custom(KIND_AGENT_SESSION_TRANSCRIPT as u16),
        content.to_owned(),
    )
    .tags(tags.into_iter().map(|tag| Tag::parse(tag).expect("tag")))
    .sign_with_keys(agent)
    .expect("sign")
}

async fn submit(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    keys: &Keys,
    event: Event,
) -> Result<bool, IngestError> {
    ingest_event(state, tenant, event, http_auth(keys))
        .await
        .map(|result| result.accepted)
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn ingest_admits_ciphertext_parts_for_the_owner_or_a_live_gateway_only() {
    let state = state().await;
    let tenant = community(&state, "agent-transcript").await;
    let (owner, agent, gateway, stranger) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    let agent_bytes = agent.public_key().to_bytes().to_vec();
    for user in [&owner, &agent] {
        state
            .db
            .ensure_user(tenant.community(), &user.public_key().to_bytes())
            .await
            .expect("user");
    }
    assert!(state
        .db
        .set_agent_owner(
            tenant.community(),
            &agent_bytes,
            &owner.public_key().to_bytes()
        )
        .await
        .expect("owner"));
    let channel = create_channel(&state, &tenant, &owner, &[]).await;
    let add_agent = EventBuilder::new(Kind::Custom(9000), "")
        .tags([
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .expect("sign 9000");
    assert!(submit(&state, &tenant, &owner, add_agent).await.unwrap());
    let root = EventId::from_hex(&"ab".repeat(32)).unwrap();

    // The owner's part is stored.
    assert!(submit(
        &state,
        &tenant,
        &agent,
        part(&agent, owner.public_key(), channel, root)
    )
    .await
    .expect("owner part accepted"));

    // Plaintext content is refused before anything is stored, even when it
    // is as long as a ciphertext (so only the base64/version check catches it).
    let canonical_tags = |recipient: PublicKey| {
        vec![
            vec!["p".to_string(), recipient.to_hex()],
            vec!["e".into(), root.to_hex(), "".into(), "root".into()],
            vec!["h".into(), channel.to_string()],
            vec!["turn".into(), "turn-1".into(), "0".into(), "1".into()],
        ]
    };
    let words = format!(
        r#"{{"v":1,"updates":[{{"sessionUpdate":"agent_message_chunk","text":"{}"}}]}}"#,
        "secret plan ".repeat(20)
    );
    assert!(buzz_core::observer::content_looks_like_nip44(&words));
    let plaintext = raw(&agent, &words, canonical_tags(owner.public_key()));
    match submit(&state, &tenant, &agent, plaintext).await {
        Err(IngestError::Rejected(message)) => {
            assert!(
                message.starts_with("invalid: kind 44201 content"),
                "{message}"
            )
        }
        other => panic!("plaintext transcript must be rejected, got {other:?}"),
    }

    // A malformed envelope is refused: no turn tag.
    let mut no_turn = canonical_tags(owner.public_key());
    no_turn.pop();
    let content = encrypt_part(&agent, &owner.public_key(), &payload(root)).unwrap();
    assert!(matches!(
        submit(&state, &tenant, &agent, raw(&agent, &content, no_turn)).await,
        Err(IngestError::Rejected(_))
    ));

    // A stranger, and a gateway before its grant, are not recipients.
    for recipient in [stranger.public_key(), gateway.public_key()] {
        assert!(matches!(
            submit(
                &state,
                &tenant,
                &agent,
                part(&agent, recipient, channel, root)
            )
            .await,
            Err(IngestError::AuthFailed(_))
        ));
    }

    // Once the owner delegates to the gateway, its part is stored.
    let now = nostr::Timestamp::now().as_secs();
    let grant = buzz_sdk::gateway::Delegation::grant(gateway.public_key(), now + 3_600, now)
        .unwrap()
        .sign_with_keys(&owner)
        .unwrap();
    let owner_auth = IngestAuth::Http {
        pubkey: owner.public_key(),
        scopes: vec![Scope::UsersWrite],
        auth_method: HttpAuthMethod::Nip98,
    };
    assert!(
        ingest_event(&state, &tenant, grant, owner_auth)
            .await
            .expect("grant")
            .accepted
    );
    assert!(submit(
        &state,
        &tenant,
        &agent,
        part(&agent, gateway.public_key(), channel, root)
    )
    .await
    .expect("gateway part accepted"));
}
