use super::*;
use buzz_backend_kubernetes::sandbox_fake;
use nostr::{nips::nip19::ToBech32, EventBuilder, Kind, Tag, Timestamp};
use std::sync::{Arc, Mutex};

fn owner() -> Keys {
    Keys::parse(&"33".repeat(32)).unwrap()
}
fn agent() -> Keys {
    Keys::parse(&"22".repeat(32)).unwrap()
}
const RELAY: &str = "wss://relay.example.invalid";
const CHANNEL: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10";
const NAMESPACE: &str = "buzz-agent-staging-dev";

fn launch() -> Value {
    json!({"relay_url": RELAY, "private_key_nsec": agent().secret_key().to_bech32().unwrap(),
        "auth_tag": buzz_sdk::nip_oa::compute_auth_tag(&owner(), &agent().public_key(), "").unwrap(),
        "launch": {"command": "codex-acp", "args": [], "env": {"BUZZ_ACP_REPLAY_FLOOR":"1"},
            "policy_env": {"BUZZ_ACP_SESSION_POLICY":"thread"}, "owner_pubkey": owner().public_key()}})
}

fn mention(tags: Vec<Tag>) -> Event {
    EventBuilder::new(Kind::Custom(9), "@agent investigate this")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(agent().public_key()),
        ])
        .tags(tags)
        .custom_created_at(Timestamp::from(1_800_000_000))
        .sign_with_keys(&owner())
        .unwrap()
}

fn start(event: &Event) -> Start {
    Start::admit(
        json!({"agent_pubkey":agent().public_key(),"relay_url":RELAY,"trigger":event}),
        owner().public_key(),
        event.created_at.as_secs(),
    )
    .unwrap()
}

#[test]
fn binds_the_exact_signed_root_direct_reply_and_nested_reply() {
    let root = mention(vec![]);
    assert_eq!(start(&root).scope["thread_root"], root.id.to_hex());
    let root_id = "a".repeat(64);
    let direct = mention(vec![Tag::parse(["e", &root_id, "", "reply"]).unwrap()]);
    assert_eq!(start(&direct).scope["thread_root"], root_id);
    let nested = mention(vec![
        Tag::parse(["e", &root_id, "", "root"]).unwrap(),
        Tag::parse(["e", &"b".repeat(64), "", "reply"]).unwrap(),
    ]);
    assert_eq!(start(&nested).scope["thread_root"], root_id);
    assert_eq!(start(&nested).scope["channel_id"], CHANNEL);
}

#[test]
fn rejects_forged_wrong_owner_unaddressed_ambiguous_and_future_triggers() {
    let event = mention(vec![]);
    let value = json!({"agent_pubkey":agent().public_key(),"relay_url":RELAY,"trigger":event});
    let mut cases = vec![];
    let mut wrong_owner = value.clone();
    wrong_owner["trigger"] = json!(EventBuilder::new(Kind::Custom(9), "forged")
        .sign_with_keys(&agent())
        .unwrap());
    cases.push(wrong_owner);
    let mut tampered = value.clone();
    tampered["trigger"]["content"] = "tampered".into();
    cases.push(tampered);
    let mut wrong_agent = value.clone();
    wrong_agent["agent_pubkey"] = json!(Keys::generate().public_key());
    cases.push(wrong_agent);
    let mut extra = value.clone();
    extra["namespace"] = "victim".into();
    cases.push(extra);
    let duplicate = mention(vec![Tag::parse(["h", CHANNEL]).unwrap()]);
    cases.push(json!({"agent_pubkey":agent().public_key(),"relay_url":RELAY,"trigger":duplicate}));
    for value in cases {
        assert!(Start::admit(value, owner().public_key(), event.created_at.as_secs()).is_err());
    }
    assert!(Start::admit(value, owner().public_key(), event.created_at.as_secs() - 61).is_err());
}

#[tokio::test]
async fn durable_registration_survives_manager_restart_and_rebinds_each_thread() {
    let state: sandbox_fake::Shared = Arc::new(Mutex::new(Default::default()));
    let registration =
        Registration::new(owner().public_key(), agent().public_key(), launch()).unwrap();
    registration
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    let event = mention(vec![]);
    // A fresh client and Start know nothing about the desktop or prior process.
    let wire::Request::Deploy(request) = start(&event)
        .request(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        request.agent.private_key_nsec,
        agent().secret_key().to_bech32().unwrap()
    );
    assert_eq!(
        request.provider_config["sandbox"]["thread_root"],
        event.id.to_hex()
    );
    assert_eq!(
        request.agent.launch.unwrap().env["BUZZ_ACP_REPLAY_FLOOR"],
        event.created_at.to_string()
    );
    let root = "b".repeat(64);
    let reply = mention(vec![Tag::parse(["e", &root, "", "reply"]).unwrap()]);
    let wire::Request::Deploy(request) = start(&reply)
        .request(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(request.provider_config["sandbox"]["thread_root"], root);
    assert_eq!(state.lock().unwrap().secrets.len(), 1);
}

#[tokio::test]
async fn missing_registration_and_tampered_saved_identity_fail_closed() {
    let state: sandbox_fake::Shared = Arc::new(Mutex::new(Default::default()));
    let event = mention(vec![]);
    let error = start(&event)
        .request(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap_err();
    assert_eq!(error["code"], "launch_not_registered");
    let mut registration =
        Registration::new(owner().public_key(), agent().public_key(), launch()).unwrap();
    registration.payload["private_key_nsec"] = owner().secret_key().to_bech32().unwrap().into();
    registration
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    let error = start(&event)
        .request(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap_err();
    assert_eq!(error["code"], "launch_store_failed");
    assert!(!error.to_string().contains("nsec"));
    assert!(state.lock().unwrap().sandboxes.is_empty());
}

#[tokio::test]
async fn successful_desktop_deploys_refresh_settings_atomically_and_report_save_failure() {
    let state: sandbox_fake::Shared = Arc::new(Mutex::new(Default::default()));
    let mut registration =
        Registration::new(owner().public_key(), agent().public_key(), launch()).unwrap();
    registration
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    registration.payload["launch"]["env"]["BUZZ_ACP_MODEL"] = "new-model".into();
    registration
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().secrets.len(), 1);
    let wire::Request::Deploy(request) = start(&mention(vec![]))
        .request(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        request.agent.launch.unwrap().env["BUZZ_ACP_MODEL"],
        "new-model"
    );
    let new_state: sandbox_fake::Shared = Arc::new(Mutex::new(sandbox_fake::State {
        fail_secret_creates: 1,
        ..Default::default()
    }));
    assert_eq!(
        registration
            .save(sandbox_fake::client(&new_state), NAMESPACE)
            .await
            .unwrap_err()["code"],
        "launch_store_failed"
    );
}

#[tokio::test]
async fn another_community_or_owner_cannot_read_the_registered_identity() {
    let state: sandbox_fake::Shared = Arc::new(Mutex::new(Default::default()));
    Registration::new(owner().public_key(), agent().public_key(), launch())
        .unwrap()
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    let mut other = start(&mention(vec![]));
    other.relay = "wss://other.example.invalid".into();
    assert_eq!(
        other
            .request(sandbox_fake::client(&state), NAMESPACE)
            .await
            .unwrap_err()["code"],
        "launch_not_registered"
    );
    let mut other = start(&mention(vec![]));
    other.owner = Keys::generate().public_key();
    assert_eq!(
        other
            .request(sandbox_fake::client(&state), NAMESPACE)
            .await
            .unwrap_err()["code"],
        "launch_not_registered"
    );
    assert!(state.lock().unwrap().sandboxes.is_empty());
}

#[tokio::test]
async fn manager_admits_only_the_owner_key_and_confines_mobile_starts() {
    use crate::manager::{Admission, Manager, ManagerConfig, Work};
    use buzz_session_control::{build_request, Op, Request as Envelope};
    let manager_keys = Keys::parse(&"11".repeat(32)).unwrap();
    let manager = Manager::new(manager_keys.clone(), ManagerConfig::parse(&json!({
        "version":1,"checkpoint_bucket":"fixture-checkpoints","developers":{"dev":{
            "owner_pubkey":owner().public_key(),"namespace":NAMESPACE,
            "profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"}}}).to_string()).unwrap(),
        sandbox_fake::client(&Default::default()), crate::enroll::tests::offline(Default::default()));
    let event = mention(vec![]);
    let payload = json!({"agent_pubkey":agent().public_key(),"relay_url":RELAY,"trigger":event});
    let request = Envelope::new(Op::Start, "mobile-start", payload);
    let sent = build_request(
        &owner(),
        &manager_keys.public_key(),
        None,
        &request,
        event.created_at,
    )
    .unwrap();
    let Admission::Start { job, .. } = manager.admit(&sent, event.created_at) else {
        panic!()
    };
    let Work::Start {
        target:
            provider::Target::Managed {
                namespace,
                owner: target_owner,
                agent: target_agent,
                ..
            },
        ..
    } = job.work
    else {
        panic!()
    };
    assert_eq!(namespace, NAMESPACE);
    assert_eq!(target_owner, owner().public_key());
    assert_eq!(target_agent, agent().public_key());
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner(), &agent().public_key(), "").unwrap();
    let agent_signed = build_request(
        &agent(),
        &manager_keys.public_key(),
        Some(&auth),
        &request,
        event.created_at,
    )
    .unwrap();
    let Admission::Done { response, .. } = manager.admit(&agent_signed, event.created_at) else {
        panic!()
    };
    assert_eq!(response["code"], "signer_mismatch");
}

#[test]
fn opens_a_real_dart_signed_and_nip44_encrypted_start_fixture() {
    let event: Event = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../buzz-session-control/tests/fixtures/start.request.event.json"
    )))
    .unwrap();
    let manager = Keys::parse(&"11".repeat(32)).unwrap();
    let opened = buzz_session_control::open_request(&event, &manager).unwrap();
    assert_eq!(opened.request.op, buzz_session_control::Op::Start);
    assert_eq!(opened.signer, owner().public_key());
    assert!(opened.auth_tag.is_none());
    let start = Start::admit(
        opened.request.payload,
        opened.signer,
        opened.created_at.as_secs(),
    )
    .unwrap();
    assert_eq!(start.agent, agent().public_key());
    assert_eq!(start.replay_floor, 1_800_000_000);
}

#[tokio::test]
async fn desktop_relay_paths_and_queries_register_without_merging_communities() {
    let relay = "wss://relay.example.invalid/community-a?network=one";
    let state: sandbox_fake::Shared = Arc::new(Mutex::new(Default::default()));
    let mut payload = launch();
    payload["relay_url"] = relay.into();
    Registration::new(owner().public_key(), agent().public_key(), payload)
        .unwrap()
        .save(sandbox_fake::client(&state), NAMESPACE)
        .await
        .unwrap();
    for endpoint in [
        relay,
        "wss://relay.example.invalid/community-b?network=one",
        "wss://relay.example.invalid/community-a?network=two",
    ] {
        let event = mention(vec![]);
        let start = Start::admit(
            json!({"agent_pubkey":agent().public_key(),"relay_url":endpoint,"trigger":event}),
            owner().public_key(),
            event.created_at.as_secs(),
        )
        .unwrap();
        let result = start.request(sandbox_fake::client(&state), NAMESPACE).await;
        if endpoint == relay {
            let wire::Request::Deploy(request) = result.unwrap() else {
                panic!()
            };
            assert_eq!(request.agent.relay_url, relay);
        } else {
            assert_eq!(result.unwrap_err()["code"], "launch_not_registered");
        }
    }
}

#[test]
fn invalid_desktop_registration_has_a_deploy_specific_refusal_without_url_secrets() {
    let mut payload = launch();
    payload["relay_url"] = "https://relay.example.invalid?token=private".into();
    let Err(error) = Registration::new(owner().public_key(), agent().public_key(), payload) else {
        panic!()
    };
    assert_eq!(error["code"], "invalid_launch_registration");
    assert!(!error.to_string().contains("private"));
}
