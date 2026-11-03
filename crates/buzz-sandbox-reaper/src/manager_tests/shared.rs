use super::*;

fn task_request() -> Sent {
    let trigger = EventBuilder::new(Kind::Custom(9), "teammate task")
        .tags([
            Tag::parse(["h", "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10"]).unwrap(),
            Tag::public_key(agent().public_key()),
        ])
        .sign_with_keys(&stranger())
        .unwrap();
    Sent {
        op: Op::Admit,
        payload: json!({"relay_url":"wss://relay.example.invalid","trigger":trigger}),
        ..Sent::deploy()
    }
}

#[tokio::test]
async fn shared_admission_is_agent_attested_and_confined_to_a_registered_identity() {
    let manager = manager();
    let sent = task_request();
    let Admission::Start { job, .. } = manager.admit(&sent.event(), now()) else {
        panic!("task was not admitted for verification")
    };
    let Work::Admit {
        owner: admitted_owner,
        agent: admitted_agent,
        namespace,
        request,
    } = &job.work
    else {
        panic!("shared task acquired a lifecycle capability")
    };
    assert_eq!(*admitted_owner, owner().public_key());
    assert_eq!(*admitted_agent, agent().public_key());
    assert_eq!(namespace, NAMESPACE);
    assert_eq!(request.trigger.pubkey, stranger().public_key());
    let response = job.work.run(&manager).await;
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["code"], "shared_task_denied",
        "unregistered identities must fail before connecting to the requested relay"
    );
}

#[tokio::test]
async fn shared_grants_cannot_replace_owner_identity_or_inject_lifecycle_payloads() {
    let manager = manager();
    let mut sent = task_request();
    sent.auth = None;
    assert_eq!(admit(&manager, &sent), "unauthorized_owner");
    let mut sent = task_request();
    sent.payload["provider_config"] = json!({"namespace":"someone-else"});
    assert_eq!(admit(&manager, &sent), "shared_task_denied");
    let mut sent = task_request();
    sent.op = Op::Start;
    assert_eq!(admit(&manager, &sent), "signer_mismatch");
}
