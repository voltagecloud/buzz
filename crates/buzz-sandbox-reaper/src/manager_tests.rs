use super::*;
use buzz_session_control::{build_request, Request as Envelope, KIND};
use nostr::nips::nip19::ToBech32;
use nostr::nips::nip44;
use nostr::{EventBuilder, Kind, Tag};
use serde_json::{json, Value};

const NAMESPACE: &str = "buzz-agent-staging-dev";

fn keys(byte: &str) -> Keys {
    Keys::parse(&byte.repeat(32)).unwrap()
}

/// The manager, developer `dev`'s agent and owner, the owner of
/// `another-dev` (a stranger to `dev`), and an unconfigured key.
fn manager_keys() -> Keys {
    keys("11")
}
fn agent() -> Keys {
    keys("22")
}
fn owner() -> Keys {
    keys("33")
}
fn stranger() -> Keys {
    keys("44")
}
fn other_agent() -> Keys {
    keys("55")
}

fn now() -> Timestamp {
    Timestamp::from(1_800_000_000)
}

fn manager() -> Manager {
    let config = ManagerConfig::parse(
        &config_json(&[("dev", &owner()), ("another-dev", &stranger())]).to_string(),
    )
    .unwrap();
    let client = buzz_backend_kubernetes::sandbox_fake::client(&Default::default());
    let enrollment = crate::enroll::tests::offline(Default::default());
    Manager::new(manager_keys(), config, client, enrollment)
}

/// One attestation per owner, agent and conditions: signing is randomized,
/// and a retry resends the same payload.
fn tag(owner: &Keys, agent: &Keys, conditions: &str) -> String {
    static TAGS: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
        std::sync::LazyLock::new(Mutex::default);
    let key = format!("{}:{}:{conditions}", owner.public_key(), agent.public_key());
    TAGS.lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| {
            buzz_sdk::nip_oa::compute_auth_tag(owner, &agent.public_key(), conditions).unwrap()
        })
        .clone()
}

/// A provider protocol request acting for `agent`, scoped to one thread.
fn payload(op: &str, agent: &Keys) -> Value {
    json!({
        "op": op,
        "agent": {"relay_url": "wss://relay.example.invalid",
            "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
            "auth_tag": tag(&owner(), agent, ""),
            "launch": {"owner_pubkey": owner().public_key().to_hex()}},
        "provider_config": {"namespace": "buzz-agent-staging-victim",
            "image": format!("example.invalid/sprig@sha256:{}", "a".repeat(64)),
            "sandbox": {"channel_id": "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10",
                "thread_root": "b".repeat(64)}}
    })
}

struct Sent {
    signer: Keys,
    op: Op,
    request_id: String,
    payload: Value,
    auth: Option<String>,
    created_at: Timestamp,
}

impl Sent {
    /// A deploy from the configured developer's agent, attested by its owner.
    fn deploy() -> Self {
        Self {
            signer: agent(),
            op: Op::Deploy,
            request_id: "r-1".into(),
            payload: payload("deploy", &agent()),
            auth: Some(tag(&owner(), &agent(), "")),
            created_at: now(),
        }
    }

    fn event(&self) -> Event {
        let envelope = Envelope::new(self.op, self.request_id.clone(), self.payload.clone());
        build_request(
            &self.signer,
            &manager_keys().public_key(),
            self.auth.as_deref(),
            &envelope,
            self.created_at,
        )
        .unwrap()
    }
}

/// The admission's refusal code, or a label for the other outcomes.
fn outcome(admission: &Admission) -> String {
    match admission {
        Admission::Ignore { error, .. } => format!("ignore:{}", error.label()),
        Admission::Done { response, .. } => match response["code"].as_str() {
            Some(code) => code.to_string(),
            None => format!("done:{response}"),
        },
        Admission::Joined { .. } => "joined".into(),
        Admission::Start { .. } => "start".into(),
    }
}

fn admit(manager: &Manager, sent: &Sent) -> String {
    outcome(&manager.admit(&sent.event(), now()))
}

#[tokio::test]
async fn an_attested_fresh_deploy_starts_in_the_owners_namespace() {
    let manager = manager();
    let sent = Sent::deploy();
    let event = sent.event();
    let Admission::Start {
        to,
        request_id,
        op,
        job,
    } = manager.admit(&event, now())
    else {
        panic!("not started");
    };
    assert_eq!(
        (to.requester, to.event, request_id.as_str(), op),
        (agent().public_key(), event.id, "r-1", Op::Deploy)
    );
    // Step 6: the payload's namespace is ignored.
    let Work::Provider {
        request: Request::Deploy(_),
        target:
            Target::Managed {
                namespace,
                agent: target_agent,
                owner: target_owner,
                ..
            },
    } = job.work
    else {
        panic!("not a managed deploy");
    };
    assert_eq!(namespace, NAMESPACE);
    assert_eq!(target_agent, agent().public_key());
    assert_eq!(target_owner, owner().public_key());
}

/// Step 1: an event that does not verify or is not for this manager gets
/// no reply.
#[tokio::test]
async fn unverifiable_or_unaddressed_events_are_ignored() {
    let manager = manager();
    let event = Sent::deploy().event();
    let mut tampered = serde_json::to_value(&event).unwrap();
    tampered["created_at"] = json!(now().as_secs() + 1);
    let tampered: Event = serde_json::from_value(tampered).unwrap();
    assert_eq!(
        outcome(&manager.admit(&tampered, now())),
        "ignore:bad_signature"
    );
    let elsewhere = build_request(
        &agent(),
        &stranger().public_key(),
        None,
        &Envelope::new(Op::Info, "r", json!({"op": "info"})),
        now(),
    )
    .unwrap();
    assert_eq!(
        outcome(&manager.admit(&elsewhere, now())),
        "ignore:wrong_recipient"
    );
    let plaintext = EventBuilder::new(Kind::Custom(KIND), Sent::deploy().payload.to_string())
        .tag(Tag::public_key(manager_keys().public_key()))
        .sign_with_keys(&agent())
        .unwrap();
    assert_eq!(
        outcome(&manager.admit(&plaintext, now())),
        "ignore:not_ciphertext"
    );
}

/// Step 2: the attestation must verify for the signer at the event's time,
/// and name a configured owner.
#[tokio::test]
async fn requests_without_a_configured_owners_attestation_are_refused() {
    let manager = manager();
    let cases = [
        (None, "no attestation"),
        (
            Some(tag(&other_agent(), &agent(), "")),
            "unconfigured owner",
        ),
        (Some(tag(&owner(), &other_agent(), "")), "another agent's"),
        (Some(tag(&owner(), &agent(), "created_at<1")), "expired"),
        (
            Some(tag(&owner(), &agent(), "created_at>1900000000")),
            "not yet valid",
        ),
    ];
    for (auth, case) in cases {
        let sent = Sent {
            auth,
            ..Sent::deploy()
        };
        assert_eq!(admit(&manager, &sent), "unauthorized_owner", "{case}");
    }
}

/// Step 3: the payload's agent key must be the signer's, so one attested
/// agent cannot act for another.
#[tokio::test]
async fn a_payload_for_another_agent_is_refused() {
    let manager = manager();
    let sent = Sent {
        payload: payload("deploy", &other_agent()),
        ..Sent::deploy()
    };
    assert_eq!(admit(&manager, &sent), "signer_mismatch");
    let mut unreadable = Sent::deploy();
    unreadable.payload["agent"]["private_key_nsec"] = json!("nsec1notakey");
    assert_eq!(admit(&manager, &unreadable), "signer_mismatch");
    let stop_as_deploy = Sent {
        payload: payload("stop", &agent()),
        ..Sent::deploy()
    };
    let outcome = admit(&manager, &stop_as_deploy);
    assert!(outcome.contains("does not match its payload"), "{outcome}");
    let mut mesh = Sent::deploy();
    mesh.payload["agent"]["provider"] = json!("relay-mesh");
    assert!(admit(&manager, &mesh).contains("relay-mesh"));
}

/// Step 4: created_at within 60 seconds of the manager's clock.
#[tokio::test]
async fn requests_outside_the_skew_are_refused() {
    let manager = manager();
    for (offset, expected) in [
        (-61, "stale_request"),
        (61, "stale_request"),
        (-60, "start"),
        (60, "start"),
    ] {
        let sent = Sent {
            created_at: Timestamp::from(now().as_secs().checked_add_signed(offset).unwrap()),
            request_id: format!("r{offset}"),
            ..Sent::deploy()
        };
        assert_eq!(admit(&manager, &sent), expected, "{offset:+}s");
    }
}

/// Step 5: the same event is refused the second time, even though its
/// request is still running; a retry is a new event.
#[tokio::test]
async fn a_replayed_event_is_refused() {
    let manager = manager();
    let event = Sent::deploy().event();
    assert_eq!(outcome(&manager.admit(&event, now())), "start");
    assert_eq!(outcome(&manager.admit(&event, now())), "replayed_request");
}

fn enroll(signer: Keys, auth: Option<String>, profile: Value) -> Sent {
    Sent {
        signer,
        op: Op::Enroll,
        payload: json!({"profile": profile}),
        auth,
        ..Sent::deploy()
    }
}

/// A valid profile of developer `id` owned by `owner`.
fn profile(id: &str, owner: &Keys) -> Value {
    crate::profile::tests::valid(id, &owner.public_key().to_hex())
}

/// Step 7 and enrollment steps 1 to 4: signed by a configured owner key
/// itself, for that owner and its own developer, with a valid profile.
/// Each refusal has its own code.
#[tokio::test]
async fn enroll_is_refused_unless_the_profile_is_the_signing_owners() {
    let manager = manager();
    let mine = || profile("dev", &owner());
    let mut invalid = mine();
    invalid["github"]["login"] = json!("-not-a-login");
    let mut without_developer = mine();
    without_developer
        .as_object_mut()
        .unwrap()
        .remove("developer");
    let mut empty_developer = mine();
    empty_developer["developer"] = json!("");
    let cases = [
        (
            enroll(agent(), Some(tag(&owner(), &agent(), "")), mine()),
            "signer_mismatch",
        ),
        (enroll(other_agent(), None, mine()), "unauthorized_owner"),
        (
            enroll(owner(), None, profile("dev", &stranger())),
            "profile_owner_mismatch",
        ),
        // The other developer's ID, so its ARN: refused, nothing runs.
        (
            enroll(owner(), None, profile("another-dev", &owner())),
            "profile_developer_mismatch",
        ),
        (enroll(owner(), None, invalid), "invalid_profile"),
        (
            Sent {
                payload: json!({"profile": mine(), "extra": true}),
                ..enroll(owner(), None, mine())
            },
            "invalid_profile",
        ),
        (enroll(owner(), None, mine()), "start"),
    ];
    for (sent, expected) in cases {
        assert_eq!(admit(&manager, &sent), expected);
    }
    // Without a developer ID, the signing owner's is filled in. Each runs on a
    // fresh manager: the same request ID would be refused as a replay.
    for profile in [without_developer, empty_developer] {
        assert_eq!(
            admit(&self::manager(), &enroll(owner(), None, profile)),
            "start"
        );
    }
    // The refusal is the image's message, which names no value.
    let mut incomplete = mine();
    incomplete.as_object_mut().unwrap().remove("via");
    let invalid = manager.admit(&enroll(owner(), None, incomplete).event(), now());
    let Admission::Done { response, .. } = invalid else {
        panic!("not refused");
    };
    assert_eq!(
        response["error"],
        "Profile fields are incomplete or unsupported"
    );
}

/// An accepted enroll targets the signing owner's developer, so its ARN.
#[tokio::test]
async fn enroll_runs_for_the_signing_owners_developer() {
    let manager = manager();
    let sent = enroll(stranger(), None, profile("another-dev", &stranger()));
    let Admission::Start { job, .. } = manager.admit(&sent.event(), now()) else {
        panic!("not started");
    };
    let Work::Enroll { developer, profile } = job.work else {
        panic!("not an enrollment");
    };
    assert_eq!(developer.id, "another-dev");
    assert!(developer
        .profile_secret_arn
        .ends_with("/agents/another-dev/profile-AbCdEf"));
    assert_eq!(profile["developer"], "another-dev");
}

/// An unknown envelope version or op is refused with its request id.
#[tokio::test]
async fn unknown_versions_and_ops_are_refused_with_their_request_id() {
    let manager = manager();
    let sealed = |envelope: Value| {
        let content = nip44::encrypt(
            agent().secret_key(),
            &manager_keys().public_key(),
            envelope.to_string(),
            nip44::Version::V2,
        )
        .unwrap();
        EventBuilder::new(Kind::Custom(KIND), content)
            .tag(Tag::public_key(manager_keys().public_key()))
            .sign_with_keys(&agent())
            .unwrap()
    };
    for (envelope, code) in [
        (
            json!({"v": 2, "op": "deploy", "request_id": "r-x", "payload": {}}),
            "unsupported_version",
        ),
        (
            json!({"v": 1, "op": "recover", "request_id": "r-x", "payload": {}}),
            "unsupported_op",
        ),
    ] {
        let admission = manager.admit(&sealed(envelope), now());
        assert_eq!(outcome(&admission), code);
        let Admission::Done { request_id, to, .. } = admission else {
            unreachable!()
        };
        assert_eq!(
            (request_id.as_str(), to.requester),
            ("r-x", agent().public_key())
        );
    }
}

/// Retries share one run: a duplicate of a running request joins it, and
/// one after it finished is answered from the record.
#[tokio::test]
async fn retries_of_a_request_id_join_or_replay_its_one_run() {
    let manager = manager();
    let first = Sent::deploy().event();
    let Admission::Start { job, .. } = manager.admit(&first, now()) else {
        panic!("not started");
    };
    let retry = Sent {
        created_at: Timestamp::from(now().as_secs() + 1),
        ..Sent::deploy()
    }
    .event();
    assert_eq!(outcome(&manager.admit(&retry, now())), "joined");
    // Each event differs (random NIP-44 nonce), so none is a replay.
    for _ in 2..MAX_WAITERS {
        assert_eq!(admit(&manager, &Sent::deploy()), "joined");
    }
    assert_eq!(admit(&manager, &Sent::deploy()), "manager_busy");
    let response = json!({"ok": true, "agent_id": "buzz-agent-x"});
    let waiters = manager.complete(&job.key, response.clone(), now());
    assert_eq!(waiters.len(), MAX_WAITERS);
    assert_eq!(
        &waiters[..2].iter().map(|w| w.event).collect::<Vec<_>>(),
        &[first.id, retry.id]
    );
    let late = Sent {
        created_at: Timestamp::from(now().as_secs() + 2),
        ..Sent::deploy()
    }
    .event();
    let Admission::Done {
        response: cached, ..
    } = manager.admit(&late, now())
    else {
        panic!("not answered from the record");
    };
    assert_eq!(cached, response);
    // The record expires with the replay window.
    let later = Timestamp::from(now().as_secs() + REPLAY_WINDOW_SECS + 1);
    let fresh = Sent {
        created_at: later,
        ..Sent::deploy()
    }
    .event();
    assert_eq!(outcome(&manager.admit(&fresh, later)), "start");
}

#[tokio::test]
async fn a_request_id_is_scoped_to_its_signer_and_bound_to_its_payload() {
    let manager = manager();
    assert_eq!(admit(&manager, &Sent::deploy()), "start");
    let changed = Sent {
        created_at: Timestamp::from(now().as_secs() + 1),
        payload: {
            let mut payload = payload("deploy", &agent());
            payload["provider_config"]["sandbox"]["thread_root"] = json!("c".repeat(64));
            payload
        },
        ..Sent::deploy()
    };
    assert_eq!(admit(&manager, &changed), "request_id_reused");
    let other = Sent {
        signer: other_agent(),
        payload: payload("deploy", &other_agent()),
        auth: Some(tag(&owner(), &other_agent(), "")),
        ..Sent::deploy()
    };
    assert_eq!(admit(&manager, &other), "start");
}

/// Running requests are bounded; a finished one frees its slot.
#[tokio::test]
async fn running_requests_are_bounded() {
    let manager = manager();
    let mut keys = Vec::new();
    for n in 0..MAX_RUNNING {
        let sent = Sent {
            request_id: format!("r-{n}"),
            ..Sent::deploy()
        };
        let Admission::Start { job, .. } = manager.admit(&sent.event(), now()) else {
            panic!("request {n} not started");
        };
        keys.push(job.key);
    }
    let one_more = Sent {
        request_id: "r-more".into(),
        ..Sent::deploy()
    };
    assert_eq!(admit(&manager, &one_more), "manager_busy");
    manager.complete(&keys[0], json!({"ok": true}), now());
    let again = Sent {
        created_at: Timestamp::from(now().as_secs() + 1),
        ..one_more
    };
    assert_eq!(admit(&manager, &again), "start");
}

#[tokio::test]
async fn info_is_authorized_like_any_request() {
    let manager = manager();
    let info = Sent {
        op: Op::Info,
        payload: json!({"op": "info"}),
        ..Sent::deploy()
    };
    assert_eq!(admit(&manager, &info), "start");
    let unattested = Sent {
        request_id: "r-2".into(),
        auth: None,
        ..info
    };
    assert_eq!(admit(&manager, &unattested), "unauthorized_owner");
}

/// The Terraform-rendered config for `developers` (ID and owner key).
fn config_json(developers: &[(&str, &Keys)]) -> Value {
    let developers: serde_json::Map<String, Value> = developers
        .iter()
        .map(|(id, owner)| {
            let entry = json!({"owner_pubkey": owner.public_key().to_hex(),
                "namespace": format!("buzz-agent-staging-{id}"),
                "profile_secret_arn": format!(
                    "arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/{id}/profile-AbCdEf")});
            (id.to_string(), entry)
        })
        .collect();
    json!({"version": 1, "developers": developers})
}

#[test]
fn manager_config_maps_each_owner_to_its_namespace() {
    let config = ManagerConfig::parse(
        &config_json(&[("dev", &owner()), ("another-dev", &stranger())]).to_string(),
    )
    .unwrap();
    assert_eq!(config.len(), 2);
    assert_eq!(
        config.developer(&owner().public_key()).unwrap().region,
        "us-west-2"
    );
    assert_eq!(
        config
            .developer(&owner().public_key())
            .map(|d| d.namespace.as_str()),
        Some(NAMESPACE)
    );
    assert_eq!(
        config
            .developer(&stranger().public_key())
            .map(|d| d.namespace.as_str()),
        Some("buzz-agent-staging-another-dev")
    );
    assert_eq!(
        config
            .developer(&agent().public_key())
            .map(|d| d.namespace.as_str()),
        None
    );
}

#[test]
fn manager_config_refuses_unknown_versions_and_duplicate_owners() {
    let mut value = config_json(&[("dev", &owner())]);
    for version in [json!(2), json!(0)] {
        value["version"] = version.clone();
        assert_eq!(
            ManagerConfig::parse(&value.to_string()),
            Err(ManagerConfigError::UnsupportedVersion(
                version.as_u64().unwrap()
            ))
        );
    }
    let shared = config_json(&[("dev", &owner()), ("another-dev", &owner())]);
    // BTreeMap order: "another-dev" claims the owner first.
    assert_eq!(
        ManagerConfig::parse(&shared.to_string()),
        Err(ManagerConfigError::DuplicateOwner {
            developer: "dev".into()
        })
    );
}

#[test]
fn manager_config_refuses_each_invalid_field_and_names_it() {
    let invalid = |developer: &str, field| {
        Err(ManagerConfigError::InvalidField {
            developer: developer.into(),
            field,
        })
    };
    let cases = [
        (
            "/developers/dev/owner_pubkey",
            json!("A".repeat(64)),
            invalid("dev", "owner_pubkey"),
        ),
        (
            "/developers/dev/owner_pubkey",
            json!("a".repeat(63)),
            invalid("dev", "owner_pubkey"),
        ),
        (
            "/developers/dev/namespace",
            json!("Bad_NS"),
            invalid("dev", "namespace"),
        ),
        (
            "/developers/dev/profile_secret_arn",
            json!("voltage/staging"),
            invalid("dev", "profile_secret_arn"),
        ),
        // Another developer's secret, so `dev`'s owner could never write it.
        (
            "/developers/dev/profile_secret_arn",
            json!("arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/devx/profile-AbCdEf"),
            invalid("dev", "profile_secret_arn"),
        ),
        (
            "/developers/dev/profile_secret_arn",
            json!("arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdE"),
            invalid("dev", "profile_secret_arn"),
        ),
        (
            "/developers",
            json!({}),
            Err(ManagerConfigError::NoDevelopers),
        ),
    ];
    for (pointer, bad, expected) in cases {
        let mut value = config_json(&[("dev", &owner())]);
        *value.pointer_mut(pointer).unwrap() = bad;
        assert_eq!(
            ManagerConfig::parse(&value.to_string()),
            expected,
            "{pointer}"
        );
    }
    for id in ["-bad", &"x".repeat(33)] {
        let value = config_json(&[(id, &owner())]);
        assert_eq!(ManagerConfig::parse(&value.to_string()), invalid(id, "id"));
    }
    let mut value = config_json(&[("dev", &owner())]);
    value["developers"]["dev"]["extra"] = json!(true);
    assert!(matches!(
        ManagerConfig::parse(&value.to_string()),
        Err(ManagerConfigError::Malformed(_))
    ));
    value = config_json(&[("dev", &owner())]);
    value["namespace_prefix"] = json!("buzz-agent-staging-");
    assert!(matches!(
        ManagerConfig::parse(&value.to_string()),
        Err(ManagerConfigError::Malformed(_))
    ));
}
