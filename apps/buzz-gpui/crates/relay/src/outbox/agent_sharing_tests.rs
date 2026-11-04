use super::*;
use crate::agent_sharing::{Grant, recovery_key};
use nostr_wire::JsonUtil as _;

const CHANNEL: &str = "00000000-0000-4000-8000-000000000001";

fn options(agent: &str) -> SendOptions {
    SendOptions {
        recovery: Some(Recovery {
            key: recovery_key(CHANNEL, agent),
            value: "true".into(),
        }),
        ..SendOptions::default()
    }
}

#[tokio::test(start_paused = true)]
async fn agent_grant_clock_is_strictly_newer_and_native_bytes_match_server() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let agent = Identity::ephemeral().pubkey_hex();
    let id = h
        .outbox
        .send_after(
            Grant::draft(&agent, CHANNEL, false).unwrap(),
            SendOptions::default(),
            NOW,
        )
        .unwrap();
    flush().await;
    let event = h
        .relay
        .posted()
        .into_iter()
        .find(|event| event.id == id)
        .unwrap();
    assert_eq!(event.created_at.as_secs(), NOW + 1);
    let wire = nostr_wire::Event::from_json(serde_json::to_string(&event).unwrap()).unwrap();
    assert!(!buzz_sdk::sharing::Grant::parse(&wire).unwrap().enabled);
    assert!(
        h.outbox
            .send_after(
                Grant::draft(&agent, CHANNEL, true).unwrap(),
                SendOptions::default(),
                NOW + 60
            )
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn agent_grant_lost_ack_survives_reopen_and_retries_the_same_event() {
    let h = Harness::new(Mode::Lose);
    h.outbox.ready().await.unwrap();
    let agent = Identity::ephemeral().pubkey_hex();
    let id = h
        .outbox
        .send_after(
            Grant::draft(&agent, CHANNEL, true).unwrap(),
            options(&agent),
            0,
        )
        .unwrap();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Unknown);
    let next = h.reopen(Mode::Accept);
    next.outbox.ready().await.unwrap();
    flush().await;
    assert!(next.relay.posted().is_empty());
    assert!(
        next.outbox
            .send_after(
                Grant::draft(&agent, CHANNEL, false).unwrap(),
                options(&agent),
                NOW
            )
            .is_err()
    );
    next.outbox.retry(&id, None).unwrap();
    flush().await;
    let sent = next.relay.posted();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].id, id);
    next.outbox.observe(&[verified(&sent[0])]);
    next.outbox.acknowledge(&id).await.unwrap();
    assert!(next.op(&id).recovery.is_none());
}

#[tokio::test(start_paused = true)]
async fn newer_permission_releases_old_intent_without_claiming_it_was_seen() {
    let h = Harness::new(Mode::Lose);
    h.outbox.ready().await.unwrap();
    let agent = Identity::ephemeral().pubkey_hex();
    let id = h
        .outbox
        .send_after(
            Grant::draft(&agent, CHANNEL, true).unwrap(),
            options(&agent),
            0,
        )
        .unwrap();
    flush().await;
    let draft = Grant::draft(&agent, CHANNEL, false).unwrap();
    let unsigned = h
        .identity
        .template(
            Purpose::Content,
            draft.kind,
            NOW + 1,
            draft.tags,
            draft.content,
        )
        .unwrap();
    let head = verified(&h.identity.sign(Purpose::Content, unsigned).unwrap());
    let wrong = Identity::ephemeral();
    let draft = Grant::draft(&agent, CHANNEL, false).unwrap();
    let unsigned = wrong
        .template(
            Purpose::Content,
            draft.kind,
            NOW + 2,
            draft.tags,
            draft.content,
        )
        .unwrap();
    let forged = verified(&wrong.sign(Purpose::Content, unsigned).unwrap());
    assert!(h.outbox.supersede_agent_grant(&id, &forged).await.is_err());
    h.outbox.supersede_agent_grant(&id, &head).await.unwrap();
    assert!(h.op(&id).acknowledged);
    assert_eq!(h.delivery(&id), Delivery::Unknown);
    assert!(h.op(&id).recovery.is_none());
    let count = h.relay.posted().len();
    h.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), count);
}
