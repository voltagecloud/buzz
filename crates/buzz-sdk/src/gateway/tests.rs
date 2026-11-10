use super::*;
use nostr::Keys;

const NOW: u64 = 1_800_000_000;
const DAY: u64 = 86_400;

fn keys(byte: &str) -> Keys {
    Keys::parse(&byte.repeat(32)).unwrap()
}

fn owner() -> Keys {
    keys("33")
}

fn gateway() -> Keys {
    keys("44")
}

fn grant(signer: &Keys, gateway: PublicKey, created_at: u64, expires_at: u64) -> Event {
    Delegation::build(gateway, true, Some(expires_at), created_at)
        .unwrap()
        .sign_with_keys(signer)
        .unwrap()
}

fn revoke(signer: &Keys, gateway: PublicKey, created_at: u64) -> Event {
    Delegation::revoke(gateway, created_at)
        .unwrap()
        .sign_with_keys(signer)
        .unwrap()
}

fn raw(signer: &Keys, content: &str, tags: &[&[&str]], created_at: u64) -> Event {
    EventBuilder::new(Kind::Custom(KIND_AGENT_GATEWAY_DELEGATION as u16), content)
        .tags(
            tags.iter()
                .map(|tag| Tag::parse(tag.iter().copied()).unwrap()),
        )
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(signer)
        .unwrap()
}

#[test]
fn builders_round_trip_and_refuse_unbounded_lifetimes() {
    let gw = gateway().public_key();
    let event = Delegation::grant(gw, NOW + DAY, NOW)
        .unwrap()
        .sign_with_keys(&owner())
        .unwrap();
    let (delegation, expiration) = Delegation::parse(&event).unwrap();
    assert_eq!(delegation.gateway, gw);
    assert_eq!(delegation.scope, Scope::RemoteAgents);
    assert!(delegation.enabled);
    assert_eq!(expiration, Some(NOW + DAY));
    assert_eq!(event.created_at.as_secs(), NOW);

    for expires_at in [NOW, NOW - 1, NOW + MAX_DELEGATION_SECS + 1] {
        assert!(
            Delegation::grant(gw, expires_at, NOW).is_err(),
            "{expires_at}"
        );
    }
    assert!(Delegation::grant(gw, NOW + MAX_DELEGATION_SECS, NOW).is_ok());

    let revoked = revoke(&owner(), gw, NOW);
    let (delegation, expiration) = Delegation::parse(&revoked).unwrap();
    assert!(!delegation.enabled);
    assert_eq!(expiration, None);
}

#[test]
fn only_an_exact_unexpired_owner_head_is_live() {
    let o = owner().public_key();
    let gw = gateway().public_key();
    let gw_hex = gw.to_hex();
    let good =
        format!(r#"{{"version":1,"gateway":"{gw_hex}","scope":"remote-agents","enabled":true}}"#);
    let exp = (NOW + DAY).to_string();
    let cases: Vec<(&str, Event, bool)> = vec![
        ("valid", grant(&owner(), gw, NOW, NOW + DAY), true),
        (
            "expired",
            grant(&owner(), gw, NOW - 2 * DAY, NOW - 1),
            false,
        ),
        ("expires now", grant(&owner(), gw, NOW - DAY, NOW), false),
        (
            "lifetime over the cap",
            grant(&owner(), gw, NOW - 1, NOW + MAX_DELEGATION_SECS),
            false,
        ),
        (
            "created beyond clock skew",
            grant(&owner(), gw, NOW + MAX_FUTURE_SKEW_SECS + 1, NOW + DAY),
            false,
        ),
        (
            "created within clock skew",
            grant(&owner(), gw, NOW + MAX_FUTURE_SKEW_SECS, NOW + DAY),
            true,
        ),
        ("revoked head", revoke(&owner(), gw, NOW), false),
        (
            "signed by someone else",
            grant(&keys("55"), gw, NOW, NOW + DAY),
            false,
        ),
        (
            "names another gateway",
            grant(&owner(), keys("66").public_key(), NOW, NOW + DAY),
            false,
        ),
        (
            "missing expiration",
            raw(&owner(), &good, &[&["d", &gw_hex], &["p", &gw_hex]], NOW),
            false,
        ),
        (
            "non-canonical expiration",
            raw(
                &owner(),
                &good,
                &[
                    &["d", &gw_hex],
                    &["p", &gw_hex],
                    &["expiration", &format!("0{exp}")],
                ],
                NOW,
            ),
            false,
        ),
        (
            "duplicate expiration",
            raw(
                &owner(),
                &good,
                &[
                    &["d", &gw_hex],
                    &["p", &gw_hex],
                    &["expiration", &exp],
                    &["expiration", &exp],
                ],
                NOW,
            ),
            false,
        ),
        (
            "second p tag",
            raw(
                &owner(),
                &good,
                &[
                    &["d", &gw_hex],
                    &["p", &gw_hex],
                    &["p", &keys("66").public_key().to_hex()],
                    &["expiration", &exp],
                ],
                NOW,
            ),
            false,
        ),
        (
            "d tag names another key",
            raw(
                &owner(),
                &good,
                &[
                    &["d", &keys("66").public_key().to_hex()],
                    &["p", &gw_hex],
                    &["expiration", &exp],
                ],
                NOW,
            ),
            false,
        ),
        (
            "unknown scope",
            raw(
                &owner(),
                &good.replace("remote-agents", "everything"),
                &[&["d", &gw_hex], &["p", &gw_hex], &["expiration", &exp]],
                NOW,
            ),
            false,
        ),
        (
            "unknown field",
            raw(
                &owner(),
                &good.replace("\"enabled\":true", "\"enabled\":true,\"dm\":true"),
                &[&["d", &gw_hex], &["p", &gw_hex], &["expiration", &exp]],
                NOW,
            ),
            false,
        ),
        (
            "version two",
            raw(
                &owner(),
                &good.replace("\"version\":1", "\"version\":2"),
                &[&["d", &gw_hex], &["p", &gw_hex], &["expiration", &exp]],
                NOW,
            ),
            false,
        ),
    ];
    for (name, event, expected) in cases {
        assert_eq!(live(&event, o, gw, NOW).is_some(), expected, "{name}");
    }

    let self_delegation = grant(&owner(), o, NOW, NOW + DAY);
    assert!(Delegation::parse(&self_delegation).is_err());
    let mut tampered = grant(&owner(), gw, NOW, NOW + DAY);
    tampered.content = tampered.content.replace("true", "false");
    assert!(Delegation::parse(&tampered).is_err());
}

#[test]
fn resolve_uses_the_newest_head_and_refuses_agents_and_excluded_keys() {
    let o = owner().public_key();
    let gw = gateway().public_key();
    let agent = keys("77");
    let auth = crate::nip_oa::compute_auth_tag(&owner(), &agent.public_key(), "").unwrap();
    let agent_profile = EventBuilder::new(Kind::Metadata, "{}")
        .tag(crate::nip_oa::parse_auth_tag(&auth).unwrap())
        .sign_with_keys(&agent)
        .unwrap();
    let plain_profile = EventBuilder::new(Kind::Metadata, "{}")
        .sign_with_keys(&gateway())
        .unwrap();
    // A profile with any auth tag, even a foreign or invalid one, claims agency.
    let foreign_claim = EventBuilder::new(Kind::Metadata, "{}")
        .tag(Tag::parse(["auth", &o.to_hex(), "", &"0".repeat(128)]).unwrap())
        .sign_with_keys(&gateway())
        .unwrap();

    let grant_old = grant(&owner(), gw, NOW - 10, NOW + DAY);
    let revoke_new = revoke(&owner(), gw, NOW - 5);
    let grant_newest = grant(&owner(), gw, NOW - 1, NOW + DAY);
    let agent_grant = grant(&owner(), agent.public_key(), NOW, NOW + DAY);
    let gateways = |events: &[Event], profiles: &[Event], excluded: &[PublicKey]| {
        resolve(o, events, profiles, excluded, NOW)
            .into_iter()
            .map(|live| live.gateway())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        gateways(std::slice::from_ref(&grant_old), &[], &[]),
        vec![gw]
    );
    // A stale grant returned beside a newer revocation never wins, in any order.
    assert!(gateways(&[grant_old.clone(), revoke_new.clone()], &[], &[]).is_empty());
    assert!(gateways(&[revoke_new.clone(), grant_old.clone()], &[], &[]).is_empty());
    assert_eq!(
        gateways(&[grant_old.clone(), revoke_new, grant_newest], &[], &[]),
        vec![gw]
    );
    assert_eq!(
        gateways(std::slice::from_ref(&grant_old), &[plain_profile], &[]),
        vec![gw]
    );
    assert!(gateways(std::slice::from_ref(&grant_old), &[foreign_claim], &[]).is_empty());
    // An owner's own agent can never pass as the owner's gateway.
    assert!(gateways(&[agent_grant], &[agent_profile], &[]).is_empty());
    assert!(gateways(std::slice::from_ref(&grant_old), &[], &[gw]).is_empty());
    // Another signer's head is ignored, not merged.
    assert!(gateways(&[grant(&keys("55"), gw, NOW, NOW + DAY)], &[], &[]).is_empty());
}

#[test]
fn resolve_caps_live_gateways_newest_first() {
    let o = owner().public_key();
    let heads: Vec<Event> = ["a1", "a2", "a3"]
        .iter()
        .enumerate()
        .map(|(i, byte)| {
            grant(
                &owner(),
                keys(byte).public_key(),
                NOW - 10 + i as u64,
                NOW + DAY,
            )
        })
        .collect();
    let live = resolve(o, &heads, &[], &[], NOW);
    assert_eq!(live.len(), MAX_LIVE_GATEWAYS);
    assert_eq!(live[0].gateway(), keys("a3").public_key());
    assert_eq!(live[1].gateway(), keys("a2").public_key());
    // An excluded key keeps its slot, so every consumer acts on the same set.
    let excluded = resolve(o, &heads, &[], &[keys("a3").public_key()], NOW);
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].gateway(), keys("a2").public_key());
}

#[test]
fn a_newer_unparseable_head_hides_an_older_grant() {
    let o = owner().public_key();
    let gw = gateway().public_key();
    let gw_hex = gw.to_hex();
    let older = grant(&owner(), gw, NOW - 10, NOW + DAY);
    // A newer head for the same coordinate that is not a valid delegation
    // (here: a revocation that illegally carries an expiration) is still the
    // coordinate's head, so the older grant must not resurface.
    let revoked_with_expiry = raw(
        &owner(),
        &format!(r#"{{"version":1,"gateway":"{gw_hex}","scope":"remote-agents","enabled":false}}"#),
        &[
            &["d", &gw_hex],
            &["p", &gw_hex],
            &["expiration", &(NOW + DAY).to_string()],
        ],
        NOW - 5,
    );
    assert!(Delegation::parse(&revoked_with_expiry).is_err());
    assert!(live_heads(o, &[older.clone(), revoked_with_expiry.clone()], NOW).is_empty());
    assert!(live_heads(o, &[revoked_with_expiry, older.clone()], NOW).is_empty());
    // A forged newer head (bad signature) is not the owner's and is ignored.
    let mut forged = revoke(&owner(), gw, NOW - 1);
    forged.content = forged.content.replace("false", "true");
    assert_eq!(live_heads(o, &[older, forged], NOW).len(), 1);
}

#[test]
fn signed_grant_and_delegate_verifier_bind_the_owner_signature() {
    let owner = owner();
    let gateway = gateway();
    let now = Timestamp::now().as_secs();
    let event =
        build_gateway_grant(&owner, gateway.public_key(), now + MAX_DELEGATION_SECS).unwrap();
    assert!(is_live_delegate(
        owner.public_key(),
        gateway.public_key(),
        std::slice::from_ref(&event),
        &[],
        now
    ));
    assert!(!is_live_delegate(
        keys("55").public_key(),
        gateway.public_key(),
        &[event],
        &[],
        now
    ));
    assert!(build_gateway_grant(&owner, owner.public_key(), now + DAY).is_err());
    assert!(build_gateway_grant(&owner, gateway.public_key(), now).is_err());
    assert!(
        build_gateway_grant(&owner, gateway.public_key(), now + MAX_DELEGATION_SECS + 60).is_err()
    );
}
