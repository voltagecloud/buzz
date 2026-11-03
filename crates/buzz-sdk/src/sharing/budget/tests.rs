use super::*;
use crate::sharing::task::Origin;
use nostr::Keys;

pub fn task() -> Task {
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
fn next(base: &Task, source: PublicKey, target: PublicKey, parent: EventId, byte: u8) -> Task {
    Task {
        agent: target,
        requester: source,
        trigger: EventId::from_byte_array([byte; 32]),
        handoff: true,
        origin: Origin {
            parent,
            ..base.origin
        },
        ..*base
    }
}

#[test]
fn receipts_survive_restart_and_enforce_parent_and_per_agent_limits() {
    let base = task();
    let peer = Keys::generate().public_key();
    let mut budget = Budget::new(&base, 101).unwrap();
    assert!(!budget.admit(&base, 101).unwrap());
    let first = next(&base, base.agent, peer, base.trigger, 2);
    assert!(budget.admit(&first, 102).unwrap());
    let second = next(&base, peer, base.agent, first.trigger, 3);
    assert!(budget.admit(&second, 103).unwrap());
    let mut recovered: Budget =
        serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
    assert!(!recovered.admit(&first, 104).unwrap());
    let forged = next(
        &base,
        peer,
        Keys::generate().public_key(),
        EventId::from_byte_array([77; 32]),
        4,
    );
    assert_eq!(recovered.admit(&forged, 104), Err(Error::Parent));
    let reset = next(&base, peer, base.agent, first.trigger, 5);
    assert_eq!(recovered.admit(&reset, 104), Err(Error::RepeatedAgent));
    let spoofed = next(
        &base,
        Keys::generate().public_key(),
        peer,
        second.trigger,
        6,
    );
    assert_eq!(recovered.admit(&spoofed, 104), Err(Error::Parent));
}

#[test]
fn fifth_handoff_is_refused_even_after_restart_but_exact_retry_is_free() {
    let base = task();
    let mut budget = Budget::new(&base, 101).unwrap();
    let mut prior = base;
    for byte in 2..=5 {
        let next = next(
            &base,
            prior.agent,
            Keys::generate().public_key(),
            prior.trigger,
            byte,
        );
        assert!(budget.admit(&next, 101).unwrap());
        prior = next;
    }
    let mut budget: Budget =
        serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
    assert!(!budget.admit(&prior, 102).unwrap());
    let fifth = next(
        &base,
        prior.agent,
        Keys::generate().public_key(),
        prior.trigger,
        6,
    );
    assert_eq!(budget.admit(&fifth, 102), Err(Error::Handoffs));
}

#[test]
fn expiry_cannot_replenish_an_old_request_and_different_humans_cannot_share_a_budget() {
    let base = task();
    let mut budget = Budget::new(&base, 101).unwrap();
    let expiry = base.requested_at + LIFETIME_SECONDS;
    assert!(budget.expired(expiry));
    assert_eq!(budget.admit(&base, expiry), Err(Error::Expired));
    assert!(matches!(Budget::new(&base, expiry), Err(Error::Expired)));
    let mut forged = base;
    forged.human = Keys::generate().public_key();
    assert_eq!(budget.admit(&forged, 102), Err(Error::Journal));
    forged = base;
    forged.scope.channel = uuid::Uuid::new_v4();
    assert_eq!(budget.admit(&forged, 102), Err(Error::Journal));
}
