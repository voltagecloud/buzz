//! The relay loop end to end: a fake relay that speaks NIP-01 and NIP-42 over
//! a real WebSocket and fans events out to matching subscriptions, the real
//! [`NostrWsConnection`] on both sides, and the provider's in-memory
//! apiserver behind the manager.

use super::*;
use buzz_backend_kubernetes::sandbox_fake;
use buzz_session_control::{open_reply, Request as Envelope, Status};
use futures_util::{SinkExt, StreamExt};
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Message;

/// The longest any exchange in these tests may take.
const BOUND: Duration = Duration::from_secs(20);

/// An in-memory relay: every accepted EVENT is recorded and delivered to the
/// connections whose subscription matches its kind and `p` tag.
pub(super) struct FakeRelay {
    pub(super) url: String,
    published: Arc<std::sync::Mutex<Vec<Event>>>,
    /// Pubkeys with a `#p` subscription.
    subscribed: watch::Receiver<Vec<String>>,
}

impl FakeRelay {
    pub(super) async fn start() -> Self {
        Self::start_with(true).await
    }

    /// A relay that accepts AUTH only when `admit` is set; otherwise it
    /// refuses with a message no client should repeat.
    pub(super) async fn start_with(admit: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (hub, _) = broadcast::channel::<Event>(64);
        let published = Arc::default();
        let (subscribe, subscribed) = watch::channel(Vec::new());
        let relay = (Arc::clone(&published), Arc::new(subscribe));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                tokio::spawn(connection(ws, hub.clone(), relay.clone(), admit));
            }
        });
        Self {
            url,
            published,
            subscribed,
        }
    }

    pub(super) async fn wait_for_subscriber(&mut self, pubkey: &PublicKey) {
        let hex = pubkey.to_hex();
        tokio::time::timeout(BOUND, self.subscribed.wait_for(|keys| keys.contains(&hex)))
            .await
            .unwrap()
            .unwrap();
    }

    pub(super) fn published_by(&self, pubkey: &PublicKey) -> Vec<Event> {
        let published = self.published.lock().unwrap();
        published
            .iter()
            .filter(|event| event.pubkey == *pubkey)
            .cloned()
            .collect()
    }
}

type Relay = (
    Arc<std::sync::Mutex<Vec<Event>>>,
    Arc<watch::Sender<Vec<String>>>,
);

async fn connection(
    mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    hub: broadcast::Sender<Event>,
    (published, subscribed): Relay,
    admit: bool,
) {
    let send = |frame: Value| Message::Text(frame.to_string().into());
    ws.send(send(json!(["AUTH", "challenge"]))).await.unwrap();
    let mut events = hub.subscribe();
    let mut authed: Option<PublicKey> = None;
    let mut subscription: Option<(String, Value)> = None;
    loop {
        tokio::select! {
            frame = ws.next() => {
                let Some(Ok(Message::Text(text))) = frame else { return };
                let frame: Value = serde_json::from_str(&text).unwrap();
                match frame[0].as_str().unwrap() {
                    "AUTH" => {
                        let event: Event = serde_json::from_value(frame[1].clone()).unwrap();
                        event.verify().unwrap();
                        if !admit {
                            let refusal = "restricted: relay-detail-not-for-clients";
                            ws.send(send(json!(["OK", event.id, false, refusal]))).await.unwrap();
                            continue;
                        }
                        authed = Some(event.pubkey);
                        ws.send(send(json!(["OK", event.id, true, ""]))).await.unwrap();
                    }
                    "REQ" => {
                        let id = frame[1].as_str().unwrap().to_string();
                        subscribed.send_modify(|keys| {
                            keys.extend(frame[2]["#p"].as_array().unwrap().iter()
                                .map(|key| key.as_str().unwrap().to_string()));
                        });
                        ws.send(send(json!(["EOSE", id]))).await.unwrap();
                        subscription = Some((id, frame[2].clone()));
                    }
                    "EVENT" => {
                        let event: Event = serde_json::from_value(frame[1].clone()).unwrap();
                        let accepted = authed == Some(event.pubkey) && event.verify().is_ok();
                        if accepted {
                            published.lock().unwrap().push(event.clone());
                            let _ = hub.send(event.clone());
                        }
                        ws.send(send(json!(["OK", event.id, accepted, ""]))).await.unwrap();
                    }
                    other => panic!("unexpected frame {other}"),
                }
            }
            Ok(event) = events.recv() => {
                let Some((id, filter)) = &subscription else { continue };
                let kind_matches = filter["kinds"][0] == json!(event.kind.as_u16());
                let p_matches = event.tags.iter().any(|tag| {
                    let tag = tag.as_slice();
                    tag[0] == "p" && filter["#p"].as_array().unwrap().contains(&json!(tag[1]))
                });
                if kind_matches && p_matches {
                    ws.send(send(json!(["EVENT", id, event]))).await.unwrap();
                }
            }
        }
    }
}

pub(super) fn keys(byte: &str) -> Keys {
    Keys::parse(&byte.repeat(32)).unwrap()
}

/// A client acting as one agent: connected, authenticated and subscribed to
/// its replies.
struct Agent {
    keys: Keys,
    manager: PublicKey,
    connection: NostrWsConnection,
}

impl Agent {
    async fn connect(relay: &mut FakeRelay, keys: Keys, manager: PublicKey) -> Self {
        let mut connection = NostrWsConnection::connect_authenticated(&relay.url, &keys, None)
            .await
            .unwrap();
        let filter = json!({"kinds": [KIND], "#p": [keys.public_key().to_hex()]});
        connection
            .send_raw(&json!(["REQ", "replies", filter]))
            .await
            .unwrap();
        relay.wait_for_subscriber(&keys.public_key()).await;
        Self {
            keys,
            manager,
            connection,
        }
    }

    /// Send one request and collect its replies through `done`.
    async fn request(&mut self, request: &Envelope, auth: Option<&str>) -> Vec<Reply> {
        let event = buzz_session_control::build_request(
            &self.keys,
            &self.manager,
            auth,
            request,
            Timestamp::now(),
        )
        .unwrap();
        let id = event.id;
        assert!(self.connection.send_event(event).await.unwrap().accepted);
        let mut replies = Vec::new();
        while replies.last().map(|reply: &Reply| reply.status) != Some(Status::Done) {
            let message = self.connection.next_event(BOUND).await.unwrap();
            if let RelayMessage::Event { event, .. } = message {
                replies.push(open_reply(&event, &self.keys, &self.manager, &id).unwrap());
            }
        }
        replies
    }
}

/// The deploy (or stop) payload the desktop would send for `agent`.
fn payload(op: &str, agent: &Keys, owner: &Keys, auth: &str) -> Value {
    json!({
        "op": op,
        "agent": {"relay_url": "wss://relay.example.invalid",
            "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
            "auth_tag": auth,
            "launch": {"owner_pubkey": owner.public_key().to_hex(),
                "env": {"BUZZ_CHECKPOINT_BUCKET": "bucket", "BUZZ_CHECKPOINT_PREFIX": "dev/",
                    "ANTHROPIC_API_KEY": "sk-ant-fixture-do-not-leak"}}},
        "provider_config": {"namespace": "buzz-agent-staging-victim",
            "image": format!("example.invalid/sprig@sha256:{}", "a".repeat(64)),
            "pod_options": {"active_deadline_seconds": 3600},
            "sandbox": {"channel_id": "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10",
                "thread_root": "b".repeat(64)}}
    })
}

/// One deploy and one stop through the relay, against the fake apiserver,
/// confined to the owner's namespace; every manager event is opaque,
/// refusals included.
#[tokio::test]
async fn deploy_and_stop_run_end_to_end_over_the_relay() {
    let (manager_keys, agent_keys, owner) = (keys("11"), keys("22"), keys("33"));
    let config = ManagerConfig::parse(
        &json!({"version": 1, "checkpoint_bucket": "fixture-checkpoints", "developers": {"dev": {
            "owner_pubkey": owner.public_key().to_hex(),
            "namespace": "buzz-agent-staging-dev",
            "profile_secret_arn": "arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"}}})
        .to_string(),
    )
    .unwrap();
    let cluster = sandbox_fake::Shared::default();
    let manager = Arc::new(Manager::new(
        manager_keys.clone(),
        config,
        sandbox_fake::client(&cluster),
        crate::enroll::tests::offline(Default::default()),
    ));
    let mut relay = FakeRelay::start().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let url = relay.url.clone();
    let server = tokio::spawn(async move {
        run(manager, &url, None, async {
            let _ = stopped.await;
        })
        .await;
    });
    relay.wait_for_subscriber(&manager_keys.public_key()).await;
    let mut agent = Agent::connect(&mut relay, agent_keys.clone(), manager_keys.public_key()).await;
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent_keys.public_key(), "").unwrap();

    let deploy = Envelope::new(
        Op::Deploy,
        "deploy-1",
        payload("deploy", &agent_keys, &owner, &auth),
    );
    let replies = agent.request(&deploy, Some(&auth)).await;
    let statuses: Vec<Status> = replies.iter().map(|reply| reply.status).collect();
    assert_eq!(statuses, [Status::Accepted, Status::Done]);
    let response = replies[1].response.clone().unwrap();
    assert_eq!(response["ok"], true, "{response}");
    let agent_id = response["agent_id"].as_str().unwrap().to_string();
    {
        let cluster = cluster.lock().unwrap();
        assert!(cluster.sandboxes.contains_key(&agent_id));
        let secrets: Vec<&Value> = cluster.secrets.values().collect();
        assert!(!secrets.is_empty());
        for secret in secrets {
            assert_eq!(secret["metadata"]["namespace"], "buzz-agent-staging-dev");
        }
    }

    let stop_request = Envelope::new(
        Op::Stop,
        "stop-1",
        payload("stop", &agent_keys, &owner, &auth),
    );
    let replies = agent.request(&stop_request, Some(&auth)).await;
    let response = replies.last().unwrap().response.clone().unwrap();
    assert_eq!(
        response,
        json!({"ok": true, "agent_id": agent_id, "state": "ending"})
    );
    let lifecycle = cluster.lock().unwrap().sandboxes[&agent_id]["metadata"]["annotations"]
        [buzz_backend_kubernetes::lifecycle::LIFECYCLE]
        .clone();
    assert_eq!(
        lifecycle,
        buzz_backend_kubernetes::lifecycle::LIFECYCLE_ENDING
    );

    // An unattested request is refused with a reply as opaque as the others.
    let refused = agent
        .request(
            &Envelope::new(
                Op::Stop,
                "stop-2",
                payload("stop", &agent_keys, &owner, &auth),
            ),
            None,
        )
        .await;
    assert_eq!(refused.len(), 1);
    assert_eq!(
        refused[0].response.as_ref().unwrap()["code"],
        "unauthorized_owner"
    );

    let sent = relay.published_by(&manager_keys.public_key());
    assert_eq!(sent.len(), 5);
    for event in sent {
        let json = serde_json::to_string(&event).unwrap();
        for needle in [
            "unauthorized_owner",
            "ending",
            "agent_id",
            "deploy-1",
            &agent_id,
        ] {
            assert!(!json.contains(needle), "{needle} visible in {json}");
        }
        let tags: Vec<&str> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice()[0].as_str())
            .collect();
        assert_eq!(tags, ["p", "e", "expiration"]);
    }
    let nsec = agent_keys.secret_key().to_bech32().unwrap();
    for event in relay.published_by(&agent_keys.public_key()) {
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains(&nsec) && !json.contains("sk-ant-fixture"));
    }

    stop.send(()).unwrap();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}

/// Every string in `value`, recursively.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// `enroll` over the relay: the owner key's request is written to its
/// developer's secret and answered with the version; a refusal is a `done`
/// too. No event, in either direction, carries a profile value or a code.
#[tokio::test]
async fn enroll_runs_end_to_end_and_its_events_carry_no_profile_value() {
    use crate::enroll::{tests::FakeStore, Enrollment, GitHub};
    let (manager_keys, owner) = (keys("11"), keys("33"));
    let arn = "arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf";
    let config = ManagerConfig::parse(
        &json!({"version": 1, "checkpoint_bucket": "fixture-checkpoints", "developers": {"dev": {"owner_pubkey": owner.public_key().to_hex(),
            "namespace": "buzz-agent-staging-dev", "profile_secret_arn": arn}}})
        .to_string(),
    )
    .unwrap();
    let github =
        crate::enroll::tests::fake_github(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    let written = crate::enroll::tests::Written::default();
    let enrollment = Enrollment::new(
        GitHub::new(&github).unwrap(),
        Arc::new(FakeStore {
            written: written.clone(),
            fail: None,
        }),
    );
    let client = sandbox_fake::client(&Default::default());
    let manager = Arc::new(Manager::new(
        manager_keys.clone(),
        config,
        client,
        enrollment,
    ));
    let mut relay = FakeRelay::start().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let url = relay.url.clone();
    let server = tokio::spawn(async move {
        run(manager, &url, None, async {
            let _ = stopped.await;
        })
        .await;
    });
    relay.wait_for_subscriber(&manager_keys.public_key()).await;
    let mut client = Agent::connect(&mut relay, owner.clone(), manager_keys.public_key()).await;

    let profile = crate::profile::tests::valid("dev", &owner.public_key().to_hex());
    let request = Envelope::new(Op::Enroll, "enroll-1", json!({"profile": profile}));
    let replies = client.request(&request, None).await;
    let statuses: Vec<Status> = replies.iter().map(|reply| reply.status).collect();
    assert_eq!(statuses, [Status::Accepted, Status::Done]);
    assert_eq!(
        replies[1].response,
        Some(json!({"ok": true, "version_id": "version-1"}))
    );
    {
        let written = written.lock().unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].0, arn);
        assert_eq!(
            serde_json::from_str::<Value>(&written[0].2).unwrap(),
            profile
        );
    }

    let mut foreign = profile.clone();
    foreign["developer"] = json!("someone-else");
    let request = Envelope::new(Op::Enroll, "enroll-2", json!({"profile": foreign}));
    let refused = client.request(&request, None).await;
    assert_eq!(refused.len(), 1);
    assert_eq!(
        refused[0].response.as_ref().unwrap()["code"],
        "profile_developer_mismatch"
    );
    assert_eq!(written.lock().unwrap().len(), 1);

    let mut secrets = Vec::new();
    strings(&profile, &mut secrets);
    secrets.retain(|value| value.len() >= 5 && *value != owner.public_key().to_hex());
    secrets.extend(
        [
            "version-1",
            "profile_developer_mismatch",
            "enroll-1",
            "enroll",
        ]
        .map(String::from),
    );
    let mut events = relay.published_by(&owner.public_key());
    events.extend(relay.published_by(&manager_keys.public_key()));
    assert_eq!(events.len(), 5);
    for event in events {
        let json = serde_json::to_string(&event).unwrap();
        for secret in &secrets {
            assert!(
                !json.contains(secret.as_str()),
                "{secret} visible in {json}"
            );
        }
    }

    stop.send(()).unwrap();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}
