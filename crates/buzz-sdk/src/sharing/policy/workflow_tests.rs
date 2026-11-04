use super::*;

fn prompt(f: &Fixture, signer: &Keys, owner: PublicKey, literal: bool, at: u64) -> Event {
    let mut tags = vec![
        vec!["h".into(), f.channel.to_string()],
        vec!["p".into(), owner.to_hex()],
        vec!["p".into(), f.agent.public_key().to_hex()],
        vec!["buzz:workflow".into(), "true".into()],
        vec!["buzz:workflow-owner".into(), owner.to_hex()],
    ];
    if literal {
        tags.push(vec![
            "buzz:workflow-mention".into(),
            f.agent.public_key().to_hex(),
        ]);
    }
    signed(signer, 9, tags, "scheduled task", at)
}

fn without_grant(f: &mut Fixture) {
    f.events
        .retain(|event| !(event.kind.as_u16() == 30180 && event.pubkey == f.owner.public_key()));
}

#[test]
fn owner_workflow_wakes_private_agent_and_retains_relay_signature_provenance() {
    let mut f = Fixture::new();
    without_grant(&mut f);
    for at in [101, 102] {
        let event = prompt(&f, &f.relay, f.owner.public_key(), true, at);
        f.events.push(event.clone());
        let task = f
            .check(&event, f.owner.public_key(), f.agent.public_key())
            .unwrap();
        assert_eq!(task.requester, f.relay.public_key());
        assert_eq!(task.origin_requester, f.relay.public_key());
        assert_eq!(task.human, f.owner.public_key());
        assert_eq!(task.origin.request, event.id);
        assert_eq!(task.scope.root, event.id);
        assert!(!task.handoff);
        let mut budget = crate::sharing::budget::Budget::new(&task, 103).unwrap();
        assert!(!budget.admit(&task, 103).unwrap());
        let bytes = serde_json::to_vec(&budget).unwrap();
        let mut recovered: crate::sharing::budget::Budget = serde_json::from_slice(&bytes).unwrap();
        assert!(!recovered.admit(&task, 104).unwrap());
    }
}

#[test]
fn workflow_handoffs_keep_the_authorizing_human_and_the_original_budget() {
    let mut f = Fixture::new();
    without_grant(&mut f);
    let event = prompt(&f, &f.relay, f.owner.public_key(), true, 101);
    f.events.push(event.clone());
    let initial = f
        .check(&event, f.owner.public_key(), f.agent.public_key())
        .unwrap();
    let mut budget = crate::sharing::budget::Budget::new(&initial, 103).unwrap();
    let handoff = task::handoff(
        &event,
        f.agent.public_key(),
        f.peer.public_key(),
        "review this",
        103,
    )
    .unwrap()
    .custom_created_at(Timestamp::from(102))
    .sign_with_keys(&f.agent)
    .unwrap();
    f.events.push(handoff.clone());
    let next = f
        .check(&handoff, f.other.public_key(), f.peer.public_key())
        .unwrap();
    assert_eq!(next.requester, f.agent.public_key());
    assert_eq!(next.origin_requester, f.relay.public_key());
    assert_eq!(next.human, f.owner.public_key());
    assert_eq!(next.origin.request, event.id);
    assert!(next.handoff);
    assert!(budget.admit(&next, 103).unwrap());
}

#[test]
fn another_owners_workflow_requires_a_current_sharing_grant() {
    let mut f = Fixture::new();
    let event = prompt(&f, &f.relay, f.other.public_key(), true, 101);
    f.events.push(event.clone());
    let task = f
        .check(&event, f.owner.public_key(), f.agent.public_key())
        .unwrap();
    assert_eq!(task.human, f.other.public_key());
    assert_eq!(task.requester, f.relay.public_key());
    without_grant(&mut f);
    assert!(matches!(
        f.check(&event, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(_))
    ));
}

#[test]
fn forged_delegation_or_rendered_only_mentions_cannot_wake_a_private_agent() {
    let mut f = Fixture::new();
    without_grant(&mut f);
    for event in [
        prompt(&f, &f.other, f.owner.public_key(), true, 101),
        prompt(&f, &f.relay, f.owner.public_key(), false, 101),
        prompt(&f, &f.relay, f.peer.public_key(), true, 101),
    ] {
        f.events.push(event.clone());
        assert!(matches!(
            f.check(&event, f.owner.public_key(), f.agent.public_key()),
            Err(Error::Denied(_))
        ));
    }
}

#[test]
fn a_queued_workflow_rechecks_membership_archive_and_workspace_access() {
    for revoke in ["owner", "agent", "archive", "workspace"] {
        let mut f = Fixture::new();
        without_grant(&mut f);
        let event = prompt(&f, &f.relay, f.owner.public_key(), true, 101);
        f.events.push(event.clone());
        assert!(f
            .check(&event, f.owner.public_key(), f.agent.public_key())
            .is_ok());
        match revoke {
            "archive" => {
                f.events.retain(|e| e.kind.as_u16() != 39000);
                f.events.push(signed(
                    &f.relay,
                    39000,
                    vec![
                        vec!["d".into(), f.channel.to_string()],
                        vec!["archived".into(), "true".into()],
                    ],
                    "",
                    102,
                ));
            }
            "workspace" => {
                f.events.retain(|e| e.kind.as_u16() != 13534);
                f.events.push(signed(&f.relay, 13534, vec![], "", 102));
            }
            _ => {
                let removed = if revoke == "owner" {
                    f.owner.public_key()
                } else {
                    f.agent.public_key()
                };
                let index = f
                    .events
                    .iter()
                    .position(|e| e.kind.as_u16() == 39002)
                    .unwrap();
                let tags = f.events[index]
                    .tags
                    .iter()
                    .filter(|t| t.as_slice().get(1) != Some(&removed.to_hex()))
                    .map(|t| t.as_slice().to_vec())
                    .collect();
                f.events[index] = signed(&f.relay, 39002, tags, "", 102);
            }
        }
        assert!(
            matches!(
                f.check(&event, f.owner.public_key(), f.agent.public_key()),
                Err(Error::Denied(_))
            ),
            "{revoke}"
        );
    }
}
