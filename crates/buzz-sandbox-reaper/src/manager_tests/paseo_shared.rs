//! A managed daemon key acts for its owner only while the owner delegates it,
//! and never acquires independent human-sharing authority.
use super::*;
use base64::Engine;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

type MembershipHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
/// A `kinds` filter value the fake relay refuses with CLOSED, as an outage.
type FailKinds = Arc<Mutex<Option<Value>>>;

struct Relay {
    url: String,
    events: Arc<Mutex<Vec<Event>>>,
    after_membership: MembershipHook,
    fail_kinds: FailKinds,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Relay {
    async fn start() -> Self {
        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let captured = events.clone();
        let after_membership = Arc::new(Mutex::new(None));
        let hook = after_membership.clone();
        let fail_kinds: FailKinds = Arc::new(Mutex::new(None));
        let fail = fail_kinds.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(
                move |upgrade: Result<
                    axum::extract::ws::WebSocketUpgrade,
                    axum::extract::ws::rejection::WebSocketUpgradeRejection,
                >| {
                    let events = captured.clone();
                    let hook = hook.clone();
                    let fail = fail.clone();
                    async move {
                        use axum::response::IntoResponse;
                        match upgrade {
                            Ok(upgrade) => upgrade
                                .on_upgrade(move |socket| serve(socket, events, hook, fail))
                                .into_response(),
                            Err(_) => axum::Json(json!({"self":manager_keys().public_key()}))
                                .into_response(),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            events,
            after_membership,
            fail_kinds,
            task,
        }
    }
}
async fn serve(
    mut socket: axum::extract::ws::WebSocket,
    events: Arc<Mutex<Vec<Event>>>,
    after_membership: MembershipHook,
    fail_kinds: FailKinds,
) {
    use axum::extract::ws::Message;
    let frame = |v: Value| Message::Text(v.to_string().into());
    socket
        .send(frame(json!(["AUTH", "fixture"])))
        .await
        .unwrap();
    while let Some(Ok(Message::Text(text))) = socket.next().await {
        let input: Value = serde_json::from_str(&text).unwrap();
        match input[0].as_str().unwrap() {
            "AUTH" => {
                let event: Event = serde_json::from_value(input[1].clone()).unwrap();
                event.verify().unwrap();
                assert_eq!(event.pubkey, agent().public_key());
                socket
                    .send(frame(json!(["OK", event.id, true, ""])))
                    .await
                    .unwrap();
            }
            "REQ" => {
                let filter = &input[2];
                assert_eq!(
                    filter["consistency"], "strong",
                    "authority must read the writer"
                );
                if fail_kinds.lock().unwrap().as_ref() == Some(&filter["kinds"]) {
                    socket
                        .send(frame(json!(["CLOSED", input[1], "error: unavailable"])))
                        .await
                        .unwrap();
                    continue;
                }
                let nostr_filter: nostr::Filter = serde_json::from_value(filter.clone()).unwrap();
                let mut matches = events.lock().unwrap().iter().filter(|event| {
                    let expired = event.tags.iter().any(|tag| matches!(tag.as_slice(),[name,value] if name=="expiration" && value.parse::<u64>().is_ok_and(|v|v<=Timestamp::now().as_secs())));
                    !expired && nostr_filter.match_event(event, nostr::filter::MatchEventOptions::default())
                }).cloned().collect::<Vec<_>>();
                matches.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
                matches.truncate(filter["limit"].as_u64().unwrap_or(100) as usize);
                for event in matches {
                    socket
                        .send(frame(json!(["EVENT", input[1], event])))
                        .await
                        .unwrap();
                }
                socket.send(frame(json!(["EOSE", input[1]]))).await.unwrap();
                if filter["kinds"] == json!([13534]) {
                    if let Some(hook) = after_membership.lock().unwrap().take() {
                        hook();
                    }
                }
            }
            "CLOSE" => {}
            other => panic!("unexpected {other}"),
        }
    }
}

fn event(signer: &Keys, kind: u16, tags: Vec<Tag>, text: &str, created: u64) -> Event {
    EventBuilder::new(Kind::Custom(kind), text)
        .tags(tags)
        .custom_created_at(Timestamp::from(created))
        .sign_with_keys(signer)
        .unwrap()
}

/// A manager with shared tasks on, the developer's managed daemon gateway, and
/// the registered `agent()` shared in one channel where the owner, a teammate
/// human, the gateway, `agent()` and `other_agent()` are members. No
/// delegation head exists yet.
struct Setup {
    relay: Relay,
    cluster: buzz_backend_kubernetes::sandbox_fake::Shared,
    manager: Manager,
    developer: crate::manager::Developer,
    gateway: Keys,
    channel: uuid::Uuid,
    initial: Vec<Event>,
}

async fn setup() -> Setup {
    let relay = Relay::start().await;
    let cluster = buzz_backend_kubernetes::sandbox_fake::Shared::default();
    let mut manager = super::manager();
    manager.client = buzz_backend_kubernetes::sandbox_fake::client(&cluster);
    manager.config.shared_tasks = true;
    let developer = manager
        .config
        .developer(&owner().public_key())
        .unwrap()
        .clone();
    let secrets = crate::enroll::tests::daemon_secrets(&cluster);
    secrets
        .write(&developer, &json!({"models":{}}))
        .await
        .unwrap();
    let encoded = cluster.lock().unwrap().secrets["paseo-daemon-dev"]["data"]["BUZZ_PRIVATE_KEY"]
        .as_str()
        .unwrap()
        .to_owned();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let gateway = Keys::parse(std::str::from_utf8(&raw).unwrap()).unwrap();
    manager.enrollment = crate::enroll::tests::offline(Default::default()).with_paseo(secrets);
    crate::mobile_launch::Registration::new(
        owner().public_key(),
        agent().public_key(),
        json!({"relay_url":relay.url,"private_key_nsec":agent().secret_key().to_bech32().unwrap(),"auth_tag":tag(&owner(),&agent(),"")}),
    )
    .unwrap()
    .save(manager.client.clone(), NAMESPACE)
    .await
    .unwrap();
    let channel = uuid::Uuid::new_v4();
    let created = Timestamp::now().as_secs() - 100;
    let auth = buzz_sdk::nip_oa::parse_auth_tag(&tag(&owner(), &agent(), "")).unwrap();
    let mut initial = vec![event(&agent(), 0, vec![auth], "{}", created)];
    for key in [&owner(), &stranger(), &gateway] {
        initial.push(event(key, 0, vec![], "{}", created));
    }
    initial.push(
        buzz_sdk::sharing::Grant::builder(agent().public_key(), channel, true)
            .unwrap()
            .custom_created_at(Timestamp::from(created))
            .sign_with_keys(&owner())
            .unwrap(),
    );
    let mut roster = vec![Tag::parse(["d", &channel.to_string()]).unwrap()];
    let mut humans = vec![];
    for (key, role) in [
        (owner().public_key(), "member"),
        (stranger().public_key(), "member"),
        (gateway.public_key(), "member"),
        (agent().public_key(), "bot"),
        (other_agent().public_key(), "bot"),
    ] {
        roster.push(Tag::parse(["p", &key.to_hex(), "", role]).unwrap());
        if role == "member" {
            humans.push(Tag::parse(["member", &key.to_hex(), role]).unwrap());
        }
    }
    initial.push(event(&manager_keys(), 39002, roster, "", created));
    initial.push(event(&manager_keys(), 13534, humans, "", created));
    initial.push(event(
        &manager_keys(),
        39000,
        vec![
            Tag::parse(["d", &channel.to_string()]).unwrap(),
            Tag::parse(["t", "stream"]).unwrap(),
        ],
        "",
        created,
    ));
    Setup {
        relay,
        cluster,
        manager,
        developer,
        gateway,
        channel,
        initial,
    }
}

#[tokio::test]
async fn managed_gateways_never_fall_back_to_human_sharing_after_authority_loss() {
    let Setup {
        relay,
        cluster,
        manager,
        developer,
        gateway,
        channel,
        initial,
    } = setup().await;
    for state in ["revoked", "expired", "displaced"] {
        let now = Timestamp::now().as_secs();
        let mut heads = vec![];
        let count = if state == "displaced" { 33 } else { 1 };
        for index in 0..count {
            let key = if index == 0 {
                gateway.public_key()
            } else {
                Keys::generate().public_key()
            };
            let mut tags = vec![
                Tag::parse(["d", &key.to_hex()]).unwrap(),
                Tag::public_key(key),
            ];
            if state == "expired" {
                tags.push(Tag::expiration(Timestamp::from(now - 1)));
            } else if state == "displaced" && index == 0 {
                tags.push(Tag::expiration(Timestamp::from(now + 3600)));
            }
            heads.push(event(&owner(),30181,tags,&json!({"version":1,"gateway":key,"scope":"remote-agents","enabled":state!="revoked" && index==0}).to_string(),now-40+index));
        }
        for workflow in [false, true] {
            for (signer, expected) in [(&gateway, false), (&stranger(), true)] {
                let mut tags = vec![
                    Tag::parse(["h", &channel.to_string()]).unwrap(),
                    Tag::public_key(agent().public_key()),
                ];
                if workflow {
                    tags.extend([
                        Tag::parse(["buzz:workflow", "true"]).unwrap(),
                        Tag::parse(["buzz:workflow-owner", &signer.public_key().to_hex()]).unwrap(),
                        Tag::parse(["buzz:workflow-mention", &agent().public_key().to_hex()])
                            .unwrap(),
                    ]);
                }
                let relay_keys = manager_keys();
                let trigger = event(
                    if workflow { &relay_keys } else { signer },
                    9,
                    tags,
                    &format!("{state}/{workflow}/{}", signer.public_key()),
                    now,
                );
                *relay.events.lock().unwrap() = initial
                    .iter()
                    .chain(heads.iter())
                    .cloned()
                    .chain(std::iter::once(trigger.clone()))
                    .collect();
                let request = crate::shared_tasks::Request {
                    relay_url: relay.url.clone(),
                    trigger,
                };
                let stored_before = cluster.lock().unwrap().secrets.len();
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    manager.admit_shared(
                        owner().public_key(),
                        agent().public_key(),
                        NAMESPACE,
                        &request,
                    ),
                )
                .await
                .unwrap();
                assert_eq!(result["ok"], expected, "{state}/{workflow}: {result}");
                if !expected {
                    assert_eq!(
                        cluster.lock().unwrap().secrets.len(),
                        stored_before,
                        "denial must precede the shared budget write"
                    );
                    assert!(
                        result["error"].as_str().unwrap().contains("managed Paseo"),
                        "{result}"
                    );
                }
            }
        }
    }
    // Retirement is committed before deleting the identity Secret. A queued
    // event remains visible, but current workspace OR channel retirement must
    // refuse it even after the key no longer appears in managed identities.
    let saved_daemon = cluster
        .lock()
        .unwrap()
        .secrets
        .remove("paseo-daemon-dev")
        .unwrap();
    for retired_kind in [13534, 39002] {
        let mut current = initial.clone();
        let index = current
            .iter()
            .position(|event| event.kind.as_u16() == retired_kind)
            .unwrap();
        let retired = &current[index];
        let tags = retired
            .tags
            .iter()
            .filter(|tag| {
                let tag = tag.as_slice();
                !(matches!(tag.first().map(String::as_str), Some("p" | "member"))
                    && tag.get(1) == Some(&gateway.public_key().to_hex()))
            })
            .cloned()
            .collect();
        current[index] = event(
            &manager_keys(),
            retired_kind,
            tags,
            "",
            Timestamp::now().as_secs(),
        );
        for workflow in [false, true] {
            for (signer, expected) in [(&gateway, false), (&stranger(), true)] {
                let mut tags = vec![
                    Tag::parse(["h", &channel.to_string()]).unwrap(),
                    Tag::public_key(agent().public_key()),
                ];
                if workflow {
                    tags.extend([
                        Tag::parse(["buzz:workflow", "true"]).unwrap(),
                        Tag::parse(["buzz:workflow-owner", &signer.public_key().to_hex()]).unwrap(),
                        Tag::parse(["buzz:workflow-mention", &agent().public_key().to_hex()])
                            .unwrap(),
                    ]);
                }
                let relay_signer = manager_keys();
                let trigger = event(
                    if workflow { &relay_signer } else { signer },
                    9,
                    tags,
                    &format!("queued/{retired_kind}/{workflow}/{}", signer.public_key()),
                    Timestamp::now().as_secs() - 5,
                );
                *relay.events.lock().unwrap() = current
                    .iter()
                    .cloned()
                    .chain(std::iter::once(trigger.clone()))
                    .collect();
                let result = manager
                    .admit_shared(
                        owner().public_key(),
                        agent().public_key(),
                        NAMESPACE,
                        &crate::shared_tasks::Request {
                            relay_url: relay.url.clone(),
                            trigger,
                        },
                    )
                    .await;
                assert_eq!(
                    result["ok"], expected,
                    "retirement/{retired_kind}/{workflow}: {result}"
                );
            }
        }
    }
    // Commit retirement and remove the Secret immediately after returning the
    // old NIP43 head. Capturing identities after policy reads would miss this
    // key and admit the already-queued prompt through human sharing.
    for workflow in [false, true] {
        cluster
            .lock()
            .unwrap()
            .secrets
            .insert("paseo-daemon-dev".into(), saved_daemon.clone());
        let mut tags = vec![
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::public_key(agent().public_key()),
        ];
        if workflow {
            tags.extend([
                Tag::parse(["buzz:workflow", "true"]).unwrap(),
                Tag::parse(["buzz:workflow-owner", &gateway.public_key().to_hex()]).unwrap(),
                Tag::parse(["buzz:workflow-mention", &agent().public_key().to_hex()]).unwrap(),
            ]);
        }
        let relay_signer = manager_keys();
        let trigger = event(
            if workflow { &relay_signer } else { &gateway },
            9,
            tags,
            "queued during retirement",
            Timestamp::now().as_secs() - 5,
        );
        *relay.events.lock().unwrap() = initial
            .iter()
            .cloned()
            .chain(std::iter::once(trigger.clone()))
            .collect();
        let retired_cluster = cluster.clone();
        let retired_events = relay.events.clone();
        let gateway_key = gateway.public_key().to_hex();
        *relay.after_membership.lock().unwrap() = Some(Box::new(move || {
            let mut events = retired_events.lock().unwrap();
            let old = events
                .iter_mut()
                .find(|event| event.kind.as_u16() == 13534)
                .unwrap();
            let tags = old
                .tags
                .iter()
                .filter(|tag| tag.as_slice().get(1) != Some(&gateway_key))
                .cloned()
                .collect();
            *old = event(&manager_keys(), 13534, tags, "", Timestamp::now().as_secs());
            retired_cluster
                .lock()
                .unwrap()
                .secrets
                .remove("paseo-daemon-dev");
        }));
        let result = manager
            .admit_shared(
                owner().public_key(),
                agent().public_key(),
                NAMESPACE,
                &crate::shared_tasks::Request {
                    relay_url: relay.url.clone(),
                    trigger,
                },
            )
            .await;
        assert_eq!(result["ok"], false, "retirement race/{workflow}: {result}");
        assert!(
            result["error"].as_str().unwrap().contains("managed Paseo"),
            "{result}"
        );
        assert!(
            relay.after_membership.lock().unwrap().is_none(),
            "the membership response barrier must run"
        );
    }
    cluster.lock().unwrap().fail_secret_reads = 1;
    let bound = (owner().public_key(), developer);
    let failed = manager
        .enrollment
        .shared_daemon_keys(std::slice::from_ref(&bound))
        .await
        .unwrap_err();
    assert_eq!(failed["code"], "shared_task_retry");
    assert!(manager
        .enrollment
        .shared_daemon_keys(&vec![bound; 65])
        .await
        .is_err());
}

#[path = "ended_shared.rs"]
mod ended_shared;
/// Rows through the production admission: a live managed gateway's direct
/// prompt, and a handoff rooted in its prompt to the owner's other agent, admit
/// with the owner as the human and write the owner's budget, even when a
/// stranger's head also names the gateway. A revoked or expired gateway and an
/// owner without a profile are permanent denials; a delegation read failure is
/// a retry. When a bound owner's head names a teammate's key, the teammate's
/// direct prompt and the handoff it roots stay the teammate's own. A gateway
/// key held by two developers' Secrets is refused whichever owner delegates it.
/// Mutations: refuse every managed key in `check_shared_human` again (the old
/// rule) and the live rows fail; drop its owner binding and the two-Secret row
/// fails; trust any bound owner's heads in the policy and the teammate rows
/// fail.
#[tokio::test]
async fn a_live_managed_gateway_acts_as_its_owner_for_shared_tasks() {
    let Setup {
        relay,
        cluster,
        manager,
        gateway,
        channel,
        mut initial,
        ..
    } = setup().await;
    let now = Timestamp::now().as_secs();
    let initial_agent = other_agent();
    let auth = buzz_sdk::nip_oa::parse_auth_tag(&tag(&owner(), &initial_agent, "")).unwrap();
    initial.push(event(&initial_agent, 0, vec![auth], "{}", now - 100));
    let delegate = |owner: &Keys, key: PublicKey, created, expires| {
        buzz_sdk::gateway::Delegation::grant(key, expires, created)
            .unwrap()
            .sign_with_keys(owner)
            .unwrap()
    };
    let grant = |created, expires| delegate(&owner(), gateway.public_key(), created, expires);
    let revoke = buzz_sdk::gateway::Delegation::revoke(gateway.public_key(), now - 10)
        .unwrap()
        .sign_with_keys(&owner())
        .unwrap();
    let daemon = cluster.lock().unwrap().secrets["paseo-daemon-dev"].clone();
    for (state, handoff, refusal) in [
        ("live", false, None),
        ("live", true, None),
        ("stranger_names_gateway", false, None),
        ("stranger_names_gateway", true, None),
        ("teammate_names_requester", false, None),
        ("teammate_names_root_author", true, None),
        (
            "gateway_bound_to_two_owners",
            false,
            Some(("shared_task_denied", "managed Paseo")),
        ),
        // Judged as itself, the key needs the initial agent's sharing grant.
        ("revoked", true, Some(("shared_task_denied", "private"))),
        (
            "revoked",
            false,
            Some(("shared_task_denied", "managed Paseo")),
        ),
        ("expired", true, Some(("shared_task_denied", "private"))),
        (
            "expired",
            false,
            Some(("shared_task_denied", "managed Paseo")),
        ),
        (
            "owner_without_profile",
            true,
            Some(("shared_task_denied", "has no profile")),
        ),
        ("relay_failure", true, Some(("shared_task_retry", ""))),
    ] {
        let mut events = initial.clone();
        let mut root_signer = gateway.clone();
        match state {
            "revoked" => events.extend([grant(now - 20, now + 3600), revoke.clone()]),
            "expired" => events.push(grant(now - 50, now - 1)),
            "owner_without_profile" => {
                events.retain(|e| !(e.kind == Kind::Metadata && e.pubkey == owner().public_key()));
                events.push(grant(now - 20, now + 3600));
            }
            "stranger_names_gateway" => events.extend([
                grant(now - 20, now + 3600),
                delegate(&stranger(), gateway.public_key(), now - 15, now + 3600),
            ]),
            // The owner is bound to the managed gateway; its head naming the
            // teammate's key must not make the teammate speak for the owner.
            "teammate_names_requester" | "teammate_names_root_author" => {
                root_signer = stranger();
                events.extend([
                    grant(now - 20, now + 3600),
                    delegate(&owner(), stranger().public_key(), now - 15, now + 3600),
                    buzz_sdk::sharing::Grant::builder(initial_agent.public_key(), channel, true)
                        .unwrap()
                        .custom_created_at(Timestamp::from(now - 20))
                        .sign_with_keys(&owner())
                        .unwrap(),
                ]);
            }
            // Both developers' Secrets hold the key and both owners delegate it.
            "gateway_bound_to_two_owners" => {
                cluster
                    .lock()
                    .unwrap()
                    .secrets
                    .insert("paseo-daemon-another-dev".into(), daemon.clone());
                events.extend([
                    grant(now - 20, now + 3600),
                    delegate(&stranger(), gateway.public_key(), now - 15, now + 3600),
                ]);
            }
            _ => events.push(grant(now - 20, now + 3600)),
        }
        *relay.fail_kinds.lock().unwrap() = (state == "relay_failure").then(|| json!([30181]));
        let first = if handoff {
            initial_agent.public_key()
        } else {
            agent().public_key()
        };
        let root = event(
            &root_signer,
            9,
            vec![
                Tag::parse(["h", &channel.to_string()]).unwrap(),
                Tag::public_key(first),
            ],
            &format!("{state}/{handoff}"),
            now - 5,
        );
        events.push(root.clone());
        let request = root.id.to_hex();
        let trigger = if handoff {
            let link = buzz_sdk::sharing::task::handoff(
                &root,
                first,
                agent().public_key(),
                "please review",
                now,
            )
            .unwrap()
            .custom_created_at(Timestamp::from(now - 4))
            .sign_with_keys(&initial_agent)
            .unwrap();
            events.push(link.clone());
            link
        } else {
            root
        };
        *relay.events.lock().unwrap() = events;
        let journal_before = journal(&cluster);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            manager.admit_shared(
                owner().public_key(),
                agent().public_key(),
                NAMESPACE,
                &crate::shared_tasks::Request {
                    relay_url: relay.url.clone(),
                    trigger,
                },
            ),
        )
        .await
        .unwrap();
        cluster
            .lock()
            .unwrap()
            .secrets
            .remove("paseo-daemon-another-dev");
        let row = format!("{state}/{handoff}: {result}");
        let journal_after = journal(&cluster);
        match refusal {
            None => {
                let human = if root_signer.public_key() == gateway.public_key() {
                    owner().public_key()
                } else {
                    root_signer.public_key()
                };
                assert_eq!(result["ok"], true, "{row}");
                assert_eq!(result["human"], json!(human), "{row}");
                // The budget is the human's; its receipt keeps the actual signer.
                let budget = &journal_after[format!("{request}:{}", first.to_hex())];
                assert_eq!(budget["human"], json!(human), "{row}");
                assert_eq!(
                    budget["receipts"][0]["requester"],
                    json!(root_signer.public_key()),
                    "{row}"
                );
            }
            Some((code, reason)) => {
                assert_eq!(result["code"], code, "{row}");
                assert!(result["error"].as_str().unwrap().contains(reason), "{row}");
                assert_eq!(
                    journal_after, journal_before,
                    "refusal precedes budget: {row}"
                );
            }
        }
    }
}

/// The owner namespace's shared admission journal tasks, empty before the first.
fn journal(cluster: &buzz_backend_kubernetes::sandbox_fake::Shared) -> Value {
    let cluster = cluster.lock().unwrap();
    let Some((_, secret)) = cluster
        .secrets
        .iter()
        .find(|(name, _)| name.starts_with("buzz-shared-admissions-"))
    else {
        return json!({});
    };
    let text = match secret["stringData"].as_object() {
        Some(data) => data.values().next().unwrap().as_str().unwrap().to_owned(),
        None => {
            let encoded = secret["data"].as_object().unwrap().values().next().unwrap();
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded.as_str().unwrap())
                .unwrap();
            String::from_utf8(raw).unwrap()
        }
    };
    serde_json::from_str::<Value>(&text).unwrap()["tasks"].clone()
}
