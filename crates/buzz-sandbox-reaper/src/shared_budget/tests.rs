use super::*;
use buzz_backend_kubernetes::sandbox_fake;
use buzz_sdk::sharing::task::{Origin, Scope};
use nostr::{EventId, Keys};

fn task() -> Task {
    let human = Keys::generate().public_key();
    let agent = Keys::generate().public_key();
    let id = EventId::from_byte_array([1; 32]);
    Task {
        scope: Scope {
            channel: uuid::Uuid::new_v4(),
            root: id,
        },
        requester: human,
        agent,
        owner: human,
        origin: Origin {
            request: id,
            agent,
            parent: id,
        },
        human,
        origin_owner: human,
        trigger: id,
        handoff: false,
        requested_at: 100,
    }
}

#[tokio::test]
async fn journal_round_trip_replay_and_failed_write_preserve_budget() {
    let state = sandbox_fake::Shared::default();
    let client = sandbox_fake::client(&state);
    let base = task();
    state.lock().unwrap().fail_secret_create_responses = 1;
    assert!(matches!(
        admit(client.clone(), "agents", "ws://relay.test", &base, 101).await,
        Err(Error::Storage)
    ));
    admit(client.clone(), "agents", "ws://relay.test", &base, 101)
        .await
        .unwrap();
    let first = Task {
        handoff: true,
        requester: base.agent,
        agent: Keys::generate().public_key(),
        trigger: EventId::from_byte_array([2; 32]),
        ..base
    };
    // The real kube request path must return an error when persistence failed.
    state.lock().unwrap().fail_secret_updates = 1;
    assert!(matches!(
        admit(client.clone(), "agents", "ws://relay.test", &first, 102).await,
        Err(Error::Storage)
    ));
    admit(client.clone(), "agents", "ws://relay.test", &first, 102)
        .await
        .unwrap();
    admit(client.clone(), "agents", "ws://relay.test", &first, 103)
        .await
        .unwrap();
    let secrets = state.lock().unwrap().secrets.clone();
    assert_eq!(secrets.len(), 1);
    let stored: Secret = serde_json::from_value(secrets.values().next().unwrap().clone()).unwrap();
    let journal: Journal = serde_json::from_slice(&stored.data.unwrap()[DATA].0).unwrap();
    let encoded = serde_json::to_value(&journal).unwrap();
    assert_eq!(
        encoded["tasks"][Budget::key(&base)]["receipts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let expired = base.requested_at + buzz_sdk::sharing::budget::LIFETIME_SECONDS;
    assert!(matches!(
        admit(client, "agents", "ws://relay.test", &base, expired).await,
        Err(Error::Refused(_))
    ));
}

#[tokio::test]
async fn concurrent_distinct_handoffs_cannot_lose_a_reserved_admission() {
    let state = sandbox_fake::Shared::default();
    let client = sandbox_fake::client(&state);
    let base = task();
    admit(client.clone(), "agents", "ws://relay.test", &base, 101)
        .await
        .unwrap();
    let first = Task {
        handoff: true,
        requester: base.agent,
        agent: Keys::generate().public_key(),
        trigger: EventId::from_byte_array([2; 32]),
        ..base
    };
    let second = Task {
        agent: Keys::generate().public_key(),
        trigger: EventId::from_byte_array([3; 32]),
        ..first
    };
    let (a, b) = tokio::join!(
        admit(client.clone(), "agents", "ws://relay.test", &first, 102),
        admit(client.clone(), "agents", "ws://relay.test", &second, 102)
    );
    a.unwrap();
    b.unwrap();
    let stored: Secret = serde_json::from_value(
        state
            .lock()
            .unwrap()
            .secrets
            .values()
            .next()
            .unwrap()
            .clone(),
    )
    .unwrap();
    let journal: Journal = serde_json::from_slice(&stored.data.unwrap()[DATA].0).unwrap();
    assert_eq!(
        serde_json::to_value(journal).unwrap()["tasks"][Budget::key(&base)]["receipts"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}
