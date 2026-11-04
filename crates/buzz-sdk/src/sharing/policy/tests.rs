use super::*;
use nostr::{EventBuilder, Keys, Tag, Timestamp};
use std::{
    future::{ready, Future},
    task::{Context, Poll, Waker},
};
use uuid::Uuid;

#[path = "workflow_tests.rs"]
mod workflows;

struct Fixture {
    relay: Keys,
    owner: Keys,
    other: Keys,
    agent: Keys,
    peer: Keys,
    channel: Uuid,
    events: Vec<Event>,
    request: Event,
}
fn signed(keys: &Keys, kind: u16, tags: Vec<Vec<String>>, content: &str, time: u64) -> Event {
    EventBuilder::new(Kind::Custom(kind), content)
        .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}
impl Fixture {
    fn new() -> Self {
        let (relay, owner, other, agent, peer) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
        );
        let channel = Uuid::new_v4();
        let mut events = Vec::new();
        for human in [&owner, &other] {
            events.push(signed(human, 0, vec![], "{}", 100));
        }
        for (human, bot) in [(&owner, &agent), (&other, &peer)] {
            let auth = crate::nip_oa::compute_auth_tag(human, &bot.public_key(), "").unwrap();
            events.push(
                EventBuilder::new(Kind::Metadata, "{}")
                    .tag(crate::nip_oa::parse_auth_tag(&auth).unwrap())
                    .custom_created_at(Timestamp::from(100))
                    .sign_with_keys(bot)
                    .unwrap(),
            );
            events.push(
                Grant::builder(bot.public_key(), channel, true)
                    .unwrap()
                    .custom_created_at(Timestamp::from(100))
                    .sign_with_keys(human)
                    .unwrap(),
            );
        }
        let mut members = vec![vec!["d".into(), channel.to_string()]];
        events.push(signed(
            &relay,
            13534,
            vec![
                vec!["member".into(), owner.public_key().to_hex(), "owner".into()],
                vec![
                    "member".into(),
                    other.public_key().to_hex(),
                    "member".into(),
                ],
            ],
            "",
            100,
        ));
        for (key, role) in [
            (&owner, "member"),
            (&other, "member"),
            (&agent, "bot"),
            (&peer, "bot"),
        ] {
            members.push(vec![
                "p".into(),
                key.public_key().to_hex(),
                "".into(),
                role.into(),
            ]);
        }
        events.push(signed(&relay, 39002, members, "", 100));
        events.push(signed(
            &relay,
            39000,
            vec![
                vec!["d".into(), channel.to_string()],
                vec!["t".into(), "stream".into()],
            ],
            "",
            100,
        ));
        let request = signed(
            &other,
            9,
            vec![
                vec!["h".into(), channel.to_string()],
                vec!["p".into(), agent.public_key().to_hex()],
            ],
            "please work with the team",
            101,
        );
        events.push(request.clone());
        Self {
            relay,
            owner,
            other,
            agent,
            peer,
            channel,
            events,
            request,
        }
    }
    fn read(&self, filter: Value) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| {
                filter["kinds"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(e.kind.as_u16()))
                    && filter
                        .get("authors")
                        .is_none_or(|a| a.as_array().unwrap().contains(&json!(e.pubkey)))
                    && filter
                        .get("ids")
                        .is_none_or(|a| a.as_array().unwrap().contains(&json!(e.id)))
                    && ["h", "d"].iter().all(|tag| {
                        filter.get(format!("#{tag}")).is_none_or(|values| {
                            e.tags.iter().any(|t| {
                                t.as_slice().first().map(String::as_str) == Some(tag)
                                    && values.as_array().unwrap().contains(&json!(t.as_slice()[1]))
                            })
                        })
                    })
            })
            .cloned()
            .collect()
    }
    fn check(&self, event: &Event, owner: PublicKey, agent: PublicKey) -> Result<Task, Error> {
        complete(authorize(
            |f| ready(Ok(self.read(f))),
            self.relay.public_key(),
            owner,
            agent,
            event,
            103,
        ))
    }
}
fn complete<F: Future>(future: F) -> F::Output {
    match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("fixture futures must complete immediately"),
    }
}

#[test]
fn teammate_tasks_and_cross_owner_handoffs_preserve_actual_requester() {
    let mut f = Fixture::new();
    let initial = f
        .check(&f.request, f.owner.public_key(), f.agent.public_key())
        .unwrap();
    assert!(!initial.handoff);
    assert_eq!(initial.requester, f.other.public_key());
    assert_eq!(initial.human, f.other.public_key());
    let handoff = task::handoff(
        &f.request,
        f.agent.public_key(),
        f.peer.public_key(),
        "please review",
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
    assert!(next.handoff);
    assert_eq!(next.origin.request, f.request.id);
    assert_eq!(next.origin.agent, f.agent.public_key());
    assert_eq!(next.origin.parent, f.request.id);
    assert_eq!(next.origin_owner, f.owner.public_key());
    assert_eq!(next.requester, f.agent.public_key());
    assert_eq!(next.human, f.other.public_key());
    assert_eq!(next.scope, initial.scope);
    let returned = task::handoff(
        &handoff,
        f.peer.public_key(),
        f.agent.public_key(),
        "review result",
        103,
    )
    .unwrap()
    .custom_created_at(Timestamp::from(103))
    .sign_with_keys(&f.peer)
    .unwrap();
    f.events.push(returned.clone());
    let back = f
        .check(&returned, f.owner.public_key(), f.agent.public_key())
        .unwrap();
    assert_eq!(back.origin.request, f.request.id);
    assert_eq!(back.origin.parent, handoff.id);
    assert_eq!(back.requester, f.peer.public_key());
}

#[test]
fn every_admission_reads_current_grant_and_membership() {
    let mut f = Fixture::new();
    assert!(f
        .check(&f.request, f.owner.public_key(), f.agent.public_key())
        .is_ok());
    let old = f
        .events
        .iter()
        .position(|e| e.kind.as_u16() == 30180 && e.pubkey == f.owner.public_key())
        .unwrap();
    let saved = f.events[old].clone();
    f.events[old] = Grant::builder(f.agent.public_key(), f.channel, false)
        .unwrap()
        .custom_created_at(Timestamp::from(102))
        .sign_with_keys(&f.owner)
        .unwrap();
    assert!(matches!(
        f.check(&f.request, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(_))
    ));
    f.events[old] = saved;
    for removed in [
        f.other.public_key(),
        f.agent.public_key(),
        f.owner.public_key(),
    ] {
        let index = f
            .events
            .iter()
            .position(|e| e.kind.as_u16() == 39002)
            .unwrap();
        let prior = f.events[index].clone();
        let tags = prior
            .tags
            .iter()
            .filter(|t| t.as_slice().get(1) != Some(&removed.to_hex()))
            .map(|t| t.as_slice().to_vec())
            .collect();
        f.events[index] = signed(&f.relay, 39002, tags, "", 102);
        assert!(matches!(
            f.check(&f.request, f.owner.public_key(), f.agent.public_key()),
            Err(Error::Denied(_))
        ));
        f.events[index] = prior;
    }
}

#[test]
fn handoff_cannot_claim_an_unrelated_parent_or_mismatched_reply() {
    let mut f = Fixture::new();
    let unrelated = signed(
        &f.other,
        9,
        vec![
            vec!["h".into(), f.channel.to_string()],
            vec!["p".into(), f.agent.public_key().to_hex()],
            vec!["e".into(), f.request.id.to_hex(), "".into(), "root".into()],
        ],
        "unrelated request in the same thread",
        102,
    );
    f.events.push(unrelated.clone());
    let valid = task::handoff(
        &f.request,
        f.agent.public_key(),
        f.peer.public_key(),
        "review",
        103,
    )
    .unwrap()
    .custom_created_at(Timestamp::from(103))
    .sign_with_keys(&f.agent)
    .unwrap();
    for change_reply in [false, true] {
        let tags = valid
            .tags
            .iter()
            .map(|tag| {
                let mut t = tag.as_slice().to_vec();
                if t[0] == "buzz-parent"
                    || (change_reply
                        && t[0] == "e"
                        && t.get(3).map(String::as_str) == Some("reply"))
                {
                    t[1] = unrelated.id.to_hex();
                }
                t
            })
            .collect();
        let forged = signed(&f.agent, 9, tags, "forged ancestry", 103);
        f.events.push(forged.clone());
        assert!(matches!(
            f.check(&forged, f.other.public_key(), f.peer.public_key()),
            Err(Error::Denied(_))
        ));
    }
}

#[test]
fn absent_foreign_forged_and_unavailable_authorization_never_broaden_access() {
    let mut f = Fixture::new();
    let grant = f
        .events
        .iter()
        .position(|e| e.kind.as_u16() == 30180 && e.pubkey == f.owner.public_key())
        .unwrap();
    f.events.remove(grant);
    assert!(matches!(
        f.check(&f.request, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(_))
    ));
    f.events.push(
        Grant::builder(f.agent.public_key(), f.channel, true)
            .unwrap()
            .sign_with_keys(&f.other)
            .unwrap(),
    );
    assert!(matches!(
        f.check(&f.request, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(_))
    ));
    let unavailable = complete(authorize(
        |_| ready(Err(())),
        f.relay.public_key(),
        f.owner.public_key(),
        f.agent.public_key(),
        &f.request,
        103,
    ));
    assert!(matches!(unavailable, Err(Error::Unavailable)));
    let other_relay = Keys::generate();
    assert!(matches!(
        complete(authorize(
            |q| ready(Ok(f.read(q))),
            other_relay.public_key(),
            f.owner.public_key(),
            f.agent.public_key(),
            &f.request,
            103
        )),
        Err(Error::Unavailable)
    ));
}

#[test]
fn agent_cannot_reset_budget_as_human_or_handoff_without_ancestry() {
    let mut f = Fixture::new();
    let ordinary = signed(
        &f.peer,
        9,
        vec![
            vec!["h".into(), f.channel.to_string()],
            vec!["p".into(), f.agent.public_key().to_hex()],
        ],
        "pretend human",
        102,
    );
    f.events.push(ordinary.clone());
    assert!(matches!(
        f.check(&ordinary, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied("agent handoff requires task ancestry"))
    ));
    // Stripping the self-authored profile tag does not remove the relay's bot designation.
    let index = f
        .events
        .iter()
        .position(|e| e.kind == Kind::Metadata && e.pubkey == f.peer.public_key())
        .unwrap();
    f.events[index] = signed(&f.peer, 0, vec![], "{}", 102);
    assert!(matches!(
        f.check(&ordinary, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied("bot cannot claim human identity"))
    ));
    let index = f
        .events
        .iter()
        .position(|e| e.kind.as_u16() == 39002)
        .unwrap();
    let tags = f.events[index]
        .tags
        .iter()
        .map(|t| {
            let mut tag = t.as_slice().to_vec();
            if tag.first().map(String::as_str) == Some("p")
                && tag.get(1) == Some(&f.peer.public_key().to_hex())
            {
                tag[3] = "member".into();
            }
            tag
        })
        .collect();
    f.events[index] = signed(&f.relay, 39002, tags, "", 102);
    assert!(matches!(
        f.check(&ordinary, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(
            "human task requires a direct workspace member"
        ))
    ));
}

#[test]
fn malformed_scope_dm_and_different_threads_are_denied() {
    let f = Fixture::new();
    for tags in [
        vec![vec!["h".into(), f.channel.to_string()]],
        vec![
            vec!["h".into(), f.channel.to_string()],
            vec!["p".into(), f.agent.public_key().to_hex()],
            vec!["e".into(), f.request.id.to_hex()],
        ],
        vec![
            vec!["h".into(), f.channel.to_string().to_uppercase()],
            vec!["p".into(), f.agent.public_key().to_hex()],
        ],
    ] {
        let event = signed(&f.other, 9, tags, "bad scope", 102);
        assert!(matches!(
            f.check(&event, f.owner.public_key(), f.agent.public_key()),
            Err(Error::Denied(_))
        ));
    }
    let mut f = Fixture::new();
    let index = f
        .events
        .iter()
        .position(|e| e.kind.as_u16() == 39000)
        .unwrap();
    f.events[index] = signed(
        &f.relay,
        39000,
        vec![
            vec!["d".into(), f.channel.to_string()],
            vec!["t".into(), "dm".into()],
        ],
        "",
        102,
    );
    assert!(matches!(
        f.check(&f.request, f.owner.public_key(), f.agent.public_key()),
        Err(Error::Denied(_))
    ));
}
