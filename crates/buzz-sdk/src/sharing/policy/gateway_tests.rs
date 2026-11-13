//! A managed gateway that its bound owner currently delegates is judged as that
//! owner: every scenario rooted by the gateway must get exactly the owner-rooted
//! verdict. A gateway that is not delegated or not bound gets no authority, and
//! a delegation head alone never moves anyone else's identity.
use super::*;

struct Gateway {
    f: Fixture,
    gateway: Keys,
    /// The owner's second agent, shared in the channel like `agent`.
    mine: Keys,
}

fn delegation(owner: &Keys, gateway: PublicKey, created: u64, expires: u64) -> Event {
    crate::gateway::Delegation::grant(gateway, expires, created)
        .unwrap()
        .sign_with_keys(owner)
        .unwrap()
}

fn replace_roster(f: &mut Fixture, edit: impl FnOnce(&mut Vec<Vec<String>>)) {
    let index = f
        .events
        .iter()
        .position(|e| e.kind.as_u16() == 39002)
        .unwrap();
    let mut tags = f.events[index]
        .tags
        .iter()
        .map(|t| t.as_slice().to_vec())
        .collect();
    edit(&mut tags);
    f.events[index] = signed(&f.relay, 39002, tags, "", 102);
}

/// The gateway has no profile and no workspace membership of its own: it is a
/// posting channel member that the owner currently delegates, and the caller
/// binds it to the owner.
fn fixture() -> Gateway {
    let mut f = Fixture::new();
    let (gateway, mine) = (Keys::generate(), Keys::generate());
    let auth = crate::nip_oa::compute_auth_tag(&f.owner, &mine.public_key(), "").unwrap();
    f.events.push(
        EventBuilder::new(Kind::Metadata, "{}")
            .tag(crate::nip_oa::parse_auth_tag(&auth).unwrap())
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&mine)
            .unwrap(),
    );
    f.events.push(
        Grant::builder(mine.public_key(), f.channel, true)
            .unwrap()
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&f.owner)
            .unwrap(),
    );
    replace_roster(&mut f, |tags| {
        for (key, role) in [(&gateway, "member"), (&mine, "bot")] {
            tags.push(vec![
                "p".into(),
                key.public_key().to_hex(),
                "".into(),
                role.into(),
            ]);
        }
    });
    f.events
        .push(delegation(&f.owner, gateway.public_key(), 100, 103 + 3600));
    f.managed = vec![ManagedGateway {
        owner: f.owner.public_key(),
        key: gateway.public_key(),
    }];
    Gateway { f, gateway, mine }
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    OwnAgent,
    OwnAgentHandoff,
    TeammateAgent,
    TeammateAgentHandoff,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario::OwnAgent,
    Scenario::OwnAgentHandoff,
    Scenario::TeammateAgent,
    Scenario::TeammateAgentHandoff,
];

/// Post the scenario's request rooted by `root`, and any handoff, then admit
/// the final event for its target.
fn admit(g: &mut Gateway, scenario: Scenario, root: &Keys) -> Result<Task, Error> {
    let (first, target, target_owner, handoff) = match scenario {
        Scenario::OwnAgent => (&g.f.agent, &g.f.agent, &g.f.owner, false),
        Scenario::OwnAgentHandoff => (&g.f.agent, &g.mine, &g.f.owner, true),
        Scenario::TeammateAgent => (&g.f.peer, &g.f.peer, &g.f.other, false),
        Scenario::TeammateAgentHandoff => (&g.f.agent, &g.f.peer, &g.f.other, true),
    };
    let request = signed(
        root,
        9,
        vec![
            vec!["h".into(), g.f.channel.to_string()],
            vec!["p".into(), first.public_key().to_hex()],
        ],
        &format!("{scenario:?} from {}", root.public_key()),
        101,
    );
    g.f.events.push(request.clone());
    let trigger = if handoff {
        let link = task::handoff(
            &request,
            first.public_key(),
            target.public_key(),
            "please help",
            103,
        )
        .unwrap()
        .custom_created_at(Timestamp::from(102))
        .sign_with_keys(first)
        .unwrap();
        g.f.events.push(link.clone());
        link
    } else {
        request
    };
    g.f.check(&trigger, target_owner.public_key(), target.public_key())
}

/// Rows: own agent, own-agent handoff, teammate agent shared with the owner
/// (directly and by handoff), teammate agent not shared, owner without a
/// profile, a stranger who also names the gateway, then a revoked, expired,
/// unbound, or channel-removed gateway.
/// Mutation: never map a managed gateway to its owner and every `live` row
/// fails, because the gateway has no profile of its own.
#[test]
fn a_live_gateway_gets_exactly_its_owners_verdict() {
    for state in [
        "live",
        "unshared",
        "owner_without_profile",
        "stranger_names_gateway",
        "revoked",
        "expired",
        "unmanaged",
        "gateway_left_channel",
    ] {
        for scenario in SCENARIOS {
            let mut g = fixture();
            let (owner, other, gateway) =
                (g.f.owner.clone(), g.f.other.clone(), g.gateway.public_key());
            match state {
                "unshared" => {
                    g.f.events
                        .retain(|e| !(e.kind.as_u16() == 30180 && e.pubkey == other.public_key()))
                }
                "owner_without_profile" => {
                    g.f.events
                        .retain(|e| !(e.kind == Kind::Metadata && e.pubkey == owner.public_key()))
                }
                // Another member's head naming the key cannot claim or
                // contest it: only the bound owner's heads are read.
                "stranger_names_gateway" => {
                    g.f.events.push(delegation(&other, gateway, 101, 103 + 60))
                }
                "revoked" => g.f.events.push(
                    crate::gateway::Delegation::revoke(gateway, 102)
                        .unwrap()
                        .sign_with_keys(&owner)
                        .unwrap(),
                ),
                "expired" => {
                    g.f.events.retain(|e| e.kind.as_u16() != 30181);
                    g.f.events.push(delegation(&owner, gateway, 50, 103));
                }
                // A delegated gateway the caller does not manage is judged as
                // itself, as before gateways could act for their owners.
                "unmanaged" => g.f.managed.clear(),
                "gateway_left_channel" => replace_roster(&mut g.f, |tags| {
                    tags.retain(|t| t.get(1) != Some(&gateway.to_hex()))
                }),
                _ => {}
            }
            let gateway_keys = g.gateway.clone();
            let by_owner = admit(&mut g, scenario, &owner);
            let by_gateway = admit(&mut g, scenario, &gateway_keys);
            let row = format!("{state}/{scenario:?}: {by_owner:?} vs {by_gateway:?}");
            match state {
                "live" | "unshared" | "owner_without_profile" | "stranger_names_gateway" => {
                    match (&by_owner, &by_gateway) {
                        (Ok(owner_task), Ok(task)) => {
                            assert_eq!(task.human, owner.public_key(), "{row}");
                            assert_eq!(task.gateway, Some(gateway), "{row}");
                            assert_eq!(task.origin_requester, gateway, "{row}");
                            assert_eq!(owner_task.gateway, None, "{row}");
                            assert_eq!(task.origin_owner, owner_task.origin_owner, "{row}");
                            assert_eq!(task.handoff, owner_task.handoff, "{row}");
                            if !task.handoff {
                                assert_eq!(task.requester, gateway, "{row}");
                            }
                        }
                        (Err(owner_error), Err(error)) => assert_eq!(owner_error, error, "{row}"),
                        _ => panic!("gateway verdict differs from its owner's: {row}"),
                    }
                }
                _ => {
                    assert!(by_owner.is_ok(), "{row}");
                    assert!(matches!(by_gateway, Err(Error::Denied(_))), "{row}");
                }
            }
            let teammate = matches!(
                scenario,
                Scenario::TeammateAgent | Scenario::TeammateAgentHandoff
            );
            match state {
                "live" | "stranger_names_gateway" => assert!(by_gateway.is_ok(), "{row}"),
                "unshared" if teammate => assert_eq!(
                    by_gateway.unwrap_err(),
                    Error::Denied("agent is private in this channel"),
                    "{row}"
                ),
                "unshared" => assert!(by_gateway.is_ok(), "{row}"),
                // The writer's answer is current: never a retry.
                "owner_without_profile" => {
                    assert!(matches!(by_gateway, Err(Error::Denied(_))), "{row}")
                }
                "gateway_left_channel" => assert_eq!(
                    by_gateway.unwrap_err(),
                    Error::Denied("a gateway must remain a posting member of this channel"),
                    "{row}"
                ),
                _ => {}
            }
        }
    }
}

/// A delegation is one-sided: the named key never consents. A teammate that
/// is itself a bound owner publishes a live head naming another member's key;
/// that member's requests and the requests they root are still judged as that
/// member, never as the teammate. Non-handoff rows cover the requester, handoff
/// rows the original request's author.
/// Mutation: map a key through any bound owner's live heads instead of only
/// through its own binding, and this test fails.
#[test]
fn a_head_naming_another_members_key_never_moves_their_identity() {
    for victim_is_owner in [true, false] {
        for scenario in SCENARIOS {
            let mut g = fixture();
            let (victim, attacker) = if victim_is_owner {
                (g.f.owner.clone(), g.f.other.clone())
            } else {
                (g.f.other.clone(), g.f.owner.clone())
            };
            // The attacker's own managed gateway, listed first.
            g.f.managed.insert(
                0,
                ManagedGateway {
                    owner: attacker.public_key(),
                    key: Keys::generate().public_key(),
                },
            );
            g.f.events
                .push(delegation(&attacker, victim.public_key(), 102, 103 + 3600));
            let task = admit(&mut g, scenario, &victim);
            let row = format!("victim_is_owner={victim_is_owner}/{scenario:?}: {task:?}");
            let task = task.expect(&row);
            assert_eq!(task.human, victim.public_key(), "{row}");
            assert_eq!(task.origin_requester, victim.public_key(), "{row}");
            assert_eq!(task.gateway, None, "{row}");
            if !task.handoff {
                assert_eq!(task.requester, victim.public_key(), "{row}");
            }
        }
    }
}

/// Only a bound gateway key reads delegation heads, so a 30181 outage retries
/// its admission and leaves every other requester untouched.
#[test]
fn delegation_read_failure_retries_only_a_managed_gateway() {
    for gateway_root in [false, true] {
        let mut g = fixture();
        let root = if gateway_root {
            g.gateway.clone()
        } else {
            g.f.other.clone()
        };
        let request = signed(
            &root,
            9,
            vec![
                vec!["h".into(), g.f.channel.to_string()],
                vec!["p".into(), g.f.agent.public_key().to_hex()],
            ],
            "task",
            101,
        );
        g.f.events.push(request.clone());
        let result = complete(authorize(
            |filter| {
                ready(if filter["kinds"] == json!([30181]) {
                    Err(())
                } else {
                    Ok(g.f.read(filter))
                })
            },
            g.f.relay.public_key(),
            g.f.owner.public_key(),
            g.f.agent.public_key(),
            &request,
            &g.f.managed,
            103,
        ));
        if gateway_root {
            assert_eq!(result.unwrap_err(), Error::Unavailable);
        } else {
            assert_eq!(result.unwrap().human, g.f.other.public_key());
        }
    }
}

#[test]
fn a_gateway_workflow_acts_for_its_owner_only_as_a_literal_schedule() {
    for literal in [true, false] {
        let mut g = fixture();
        g.f.events.retain(|e| e.kind.as_u16() != 30180);
        let mut tags = vec![
            vec!["h".into(), g.f.channel.to_string()],
            vec!["p".into(), g.f.agent.public_key().to_hex()],
            vec!["buzz:workflow".into(), "true".into()],
            vec![
                "buzz:workflow-owner".into(),
                g.gateway.public_key().to_hex(),
            ],
            vec![
                "buzz:workflow-mention".into(),
                g.f.agent.public_key().to_hex(),
            ],
        ];
        if literal {
            tags.push(vec![
                crate::workflow::SCHEDULED_LITERAL_TAG.into(),
                "literal-v1".into(),
            ]);
        }
        let event = signed(&g.f.relay, 9, tags, "scheduled", 101);
        g.f.events.push(event.clone());
        let result =
            g.f.check(&event, g.f.owner.public_key(), g.f.agent.public_key());
        if literal {
            let task = result.unwrap();
            assert_eq!(task.human, g.f.owner.public_key());
            assert_eq!(task.gateway, Some(g.gateway.public_key()));
            assert_eq!(task.requester, g.f.relay.public_key());
        } else {
            assert_eq!(
                result.unwrap_err(),
                Error::Denied(
                    "a gateway's workflow acts for its owner only through a literal scheduled prompt"
                )
            );
        }
    }
}

/// The policy confirms a bound gateway with the same owner-window lookup every
/// consumer uses: a gateway displaced from the owner's newest
/// [`crate::gateway::LOOKUP_LIMIT`] heads is not delegated. The newer heads are
/// revocations, so only the window, not the live-gateway cap, displaces it.
#[test]
fn a_displaced_gateway_is_not_delegated() {
    let mut g = fixture();
    for index in 0..crate::gateway::LOOKUP_LIMIT as u64 {
        g.f.events.push(
            crate::gateway::Delegation::revoke(Keys::generate().public_key(), 101 + index)
                .unwrap()
                .sign_with_keys(&g.f.owner)
                .unwrap(),
        );
    }
    let request = signed(
        &g.gateway,
        9,
        vec![
            vec!["h".into(), g.f.channel.to_string()],
            vec!["p".into(), g.f.agent.public_key().to_hex()],
        ],
        "task",
        101,
    );
    g.f.events.push(request.clone());
    // The fixture relay ignores `limit`, so serve the newest window as the
    // writer would.
    let result = complete(authorize(
        |filter| {
            let mut events = g.f.read(filter.clone());
            events.sort_by_key(|event| std::cmp::Reverse(event.created_at));
            if let Some(limit) = filter["limit"].as_u64() {
                events.truncate(limit as usize);
            }
            ready(Ok(events))
        },
        g.f.relay.public_key(),
        g.f.owner.public_key(),
        g.f.agent.public_key(),
        &request,
        &g.f.managed,
        103,
    ));
    assert_eq!(
        result.unwrap_err(),
        Error::Denied(NO_REQUESTER_PROFILE),
        "a displaced gateway is judged as itself"
    );
}
