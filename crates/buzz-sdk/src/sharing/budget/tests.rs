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
        origin_requester: human,
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
    // `second` returned `first`'s result; a handoff returns once.
    let reset = next(&base, peer, base.agent, first.trigger, 5);
    assert_eq!(recovered.admit(&reset, 104), Err(Error::Returned));
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

#[test]
fn a_coordinator_collects_every_result_without_spending_the_handoff_budget() {
    let base = task();
    let coordinator = base.agent;
    let mut budget = Budget::new(&base, 101).unwrap();
    // Fan out to three workers, then each reports back.
    let workers: Vec<Task> = (0..3)
        .map(|n| {
            let handoff = next(
                &base,
                coordinator,
                Keys::generate().public_key(),
                base.trigger,
                10 + n,
            );
            assert!(budget.admit(&handoff, 102).unwrap());
            handoff
        })
        .collect();
    let returns: Vec<Task> = workers
        .iter()
        .enumerate()
        .map(|(n, work)| {
            let result = next(&base, work.agent, coordinator, work.trigger, 20 + n as u8);
            assert!(budget.admit(&result, 103).unwrap(), "return {n}");
            result
        })
        .collect();
    // Restart: the journal validates and an exact retry is free.
    let mut budget: Budget =
        serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
    assert!(!budget.admit(&returns[0], 104).unwrap());
    // One return per handoff, and a return cannot itself be returned.
    let again = next(&base, workers[0].agent, coordinator, workers[0].trigger, 30);
    assert_eq!(budget.admit(&again, 104), Err(Error::Returned));
    // A reply to a result is new work: budgeted, so the fourth handoff fits
    // and the fifth does not.
    let follow_up = next(&base, coordinator, workers[0].agent, returns[0].trigger, 31);
    assert!(budget.admit(&follow_up, 105).unwrap());
    let back = next(&base, workers[0].agent, coordinator, follow_up.trigger, 32);
    assert!(budget.admit(&back, 105).unwrap());
    let fifth = next(&base, coordinator, workers[1].agent, returns[1].trigger, 33);
    assert_eq!(budget.admit(&fifth, 105), Err(Error::Handoffs));
    // A worker cannot "return" to an agent that did not delegate to it.
    let stranger = next(
        &base,
        workers[1].agent,
        workers[2].agent,
        workers[1].trigger,
        34,
    );
    assert_eq!(budget.admit(&stranger, 105), Err(Error::Handoffs));
}

#[test]
fn a_forged_return_in_the_journal_is_refused() {
    let base = task();
    let peer = Keys::generate().public_key();
    let mut budget = Budget::new(&base, 101).unwrap();
    let work = next(&base, base.agent, peer, base.trigger, 2);
    assert!(budget.admit(&work, 102).unwrap());
    let mut forged: serde_json::Value = serde_json::to_value(&budget).unwrap();
    // A "return" by the delegator itself, answering its own handoff.
    forged["receipts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "event": EventId::from_byte_array([3; 32]),
            "agent": peer,
            "requester": base.agent,
            "handoff": true,
            "returns": work.trigger,
        }));
    let mut forged: Budget = serde_json::from_value(forged).unwrap();
    assert_eq!(forged.admit(&base, 103), Err(Error::Journal));
    // Journals written before returns existed still load.
    let legacy = serde_json::to_string(&budget).unwrap();
    assert!(!legacy.contains("returns"));
    assert!(serde_json::from_str::<Budget>(&legacy).is_ok());
}
