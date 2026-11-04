//! Full routing witness: a real relay, two offline owners, production manager,
//! shipped harness and CLI, and a deterministic ACP model adapter. Kubernetes
//! admission uses the existing fake API; no cloud resources or model API calls.

use super::*;
use buzz_ws_client::RelayMessage;
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};

#[path = "workflow_live.rs"]
mod workflow_live;

#[tokio::test]
#[ignore = "requires isolated local relay and built buzz-acp/buzz binaries"]
async fn shared_mentions_wake_offline_owners_agents_and_handoff_in_the_original_thread() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("buzz_sandbox_reaper=info,buzz_sandbox_reaper::mentions=debug")
        .with_test_writer()
        .try_init();
    let relay = std::env::var("BUZZ_MENTION_TEST_RELAY").unwrap();
    let url = url::Url::parse(&relay).unwrap();
    assert_eq!(url.scheme(), "ws");
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    let acp = PathBuf::from(std::env::var("BUZZ_SHARING_TEST_ACP").unwrap());
    let cli = PathBuf::from(std::env::var("BUZZ_SHARING_TEST_CLI").unwrap());
    assert!(acp.is_file() && cli.is_file());
    tokio::time::timeout(Duration::from_secs(100), exercise(&relay, &acp, &cli))
        .await
        .expect("shared-agent workflow timed out");
}

async fn publish(socket: &mut NostrWsConnection, builder: EventBuilder, signer: &Keys) -> Event {
    let event = builder.sign_with_keys(signer).unwrap();
    let reply = socket.send_event(event.clone()).await.unwrap();
    assert!(
        reply.accepted,
        "fixture publication refused: {}",
        reply.message
    );
    event
}

async fn channel_events(socket: &mut NostrWsConnection, channel: uuid::Uuid) -> Vec<Event> {
    socket
        .send_raw(&json!(["REQ","shared-fixture",{"kinds":[9,40002],"#h":[channel],"limit":100}]))
        .await
        .unwrap();
    let mut events = Vec::new();
    for _ in 0..110 {
        match socket.next_event(Duration::from_secs(5)).await.unwrap() {
            RelayMessage::Event {
                subscription_id,
                event,
            } if subscription_id == "shared-fixture" => events.push(*event),
            RelayMessage::Eose { subscription_id } if subscription_id == "shared-fixture" => {
                socket
                    .send_raw(&json!(["CLOSE", "shared-fixture"]))
                    .await
                    .unwrap();
                return events;
            }
            _ => {}
        }
    }
    panic!("bounded channel query exhausted");
}

async fn wait_message(
    socket: &mut NostrWsConnection,
    channel: uuid::Uuid,
    author: nostr::PublicKey,
    text: &str,
) -> Event {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(event) = channel_events(socket, channel)
                .await
                .into_iter()
                .find(|e| e.pubkey == author && e.content.contains(text))
            {
                return event;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {text} from {author}"))
}

fn harness(
    binaries: (&Path, &Path),
    directory: &Path,
    relay: &str,
    identity: (&Keys, &Keys),
    root: &Event,
    target: Option<nostr::PublicKey>,
) -> Child {
    let (acp, cli) = binaries;
    let (owner, agent) = identity;
    let auth = buzz_sdk::nip_oa::compute_auth_tag(owner, &agent.public_key(), "").unwrap();
    std::fs::write(
        directory.join("adapter.py"),
        include_str!("shared_agent.py"),
    )
    .unwrap();
    let log = std::fs::File::create(directory.join("harness.log")).unwrap();
    let channel = root
        .tags
        .iter()
        .find_map(|tag| tag.as_slice().strip_prefix(&["h".to_owned()]))
        .and_then(|values| values.first())
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let mut command = Command::new(acp);
    for (name, _) in std::env::vars()
        .filter(|(name, _)| name.starts_with("BUZZ_") || name.starts_with("SHARED_FIXTURE_"))
    {
        command.env_remove(name);
    }
    command
        .current_dir(directory)
        .kill_on_drop(true)
        .args([
            "--agent-command",
            "python3",
            "--agent-args",
            "adapter.py",
            "--heartbeat-interval",
            "0",
        ])
        .env("BUZZ_PRIVATE_KEY", agent.secret_key().to_secret_hex())
        .env("BUZZ_RELAY_URL", relay)
        .env("BUZZ_AUTH_TAG", auth)
        .env(
            "BUZZ_ACP_BOUND_SESSION",
            json!({"channel_id":channel,"thread_root":root.id}).to_string(),
        )
        .env(
            "BUZZ_ACP_REPLAY_FLOOR",
            root.created_at.as_secs().to_string(),
        )
        .env("BUZZ_ACP_SESSION_POLICY", "thread")
        .env("BUZZ_ACP_RESPOND_TO", "owner")
        .env("BUZZ_ACP_TRIGGER", "mentions")
        .env("BUZZ_ACP_SHARING_MANAGER", keys("11").public_key().to_hex())
        .env("SHARED_FIXTURE_CLI", cli)
        .env("SHARED_FIXTURE_CHANNEL", channel.to_string())
        .env("SHARED_FIXTURE_FROM", root.id.to_hex())
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::null())
        .stderr(log);
    if let Some(target) = target {
        command.env("SHARED_FIXTURE_TARGET", target.to_hex());
    }
    command.spawn().unwrap()
}

async fn stop(mut child: Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status(),
        )
        .await;
    }
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        child.kill().await.unwrap();
    }
}

async fn exercise(relay: &str, acp: &Path, cli: &Path) {
    let (a, b, agent_a, agent_b) = (keys("33"), keys("44"), keys("22"), keys("55"));
    let channel = uuid::Uuid::new_v4();
    let mut phone = NostrWsConnection::connect_authenticated(relay, &a, None)
        .await
        .unwrap();
    for member in [b.public_key(), keys("11").public_key()] {
        publish(
            &mut phone,
            EventBuilder::new(Kind::Custom(9030), "").tag(Tag::public_key(member)),
            &a,
        )
        .await;
    }
    let mut second = NostrWsConnection::connect_authenticated(relay, &b, None)
        .await
        .unwrap();
    for (owner, socket) in [(&a, &mut phone), (&b, &mut second)] {
        publish(socket, EventBuilder::new(Kind::Metadata, "{}"), owner).await;
    }
    publish(
        &mut phone,
        buzz_sdk::build_create_channel(
            channel,
            &format!("shared-{channel}"),
            Some(buzz_sdk::Visibility::Private),
            Some(buzz_sdk::ChannelKind::Stream),
            None,
            None,
        )
        .unwrap(),
        &a,
    )
    .await;
    publish(
        &mut phone,
        buzz_sdk::build_add_member(channel, &b.public_key().to_hex(), None).unwrap(),
        &a,
    )
    .await;
    let cluster = sandbox_fake::Shared::default();
    for (owner, agent, socket, namespace) in [
        (&a, &agent_a, &mut phone, NS),
        (&b, &agent_b, &mut second, "buzz-agent-staging-peer"),
    ] {
        let auth = buzz_sdk::nip_oa::compute_auth_tag(owner, &agent.public_key(), "").unwrap();
        let tag = buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap();
        let mut connection = NostrWsConnection::connect_authenticated(relay, agent, Some(&tag))
            .await
            .unwrap();
        publish(
            &mut connection,
            EventBuilder::new(Kind::Metadata, "{}").tag(tag),
            agent,
        )
        .await;
        publish(
            socket,
            buzz_sdk::build_add_member(
                channel,
                &agent.public_key().to_hex(),
                Some(buzz_sdk::MemberRole::Bot),
            )
            .unwrap(),
            owner,
        )
        .await;
        publish(
            socket,
            buzz_sdk::sharing::Grant::builder(agent.public_key(), channel, true).unwrap(),
            owner,
        )
        .await;
        publish(
            socket,
            EventBuilder::new(Kind::Custom(30177), "{}").tags([
                Tag::parse(["d", &agent.public_key().to_hex()]).unwrap(),
                Tag::parse(["runtime", "voltage.remote-workspace/sandbox"]).unwrap(),
            ]),
            owner,
        )
        .await;
        Registration::new(owner.public_key(),agent.public_key(),json!({"relay_url":relay,
            "private_key_nsec":agent.secret_key().to_bech32().unwrap(),"auth_tag":auth,
            "launch":{"command":"codex-acp","args":[],"env":{},"policy_env":{"BUZZ_ACP_SESSION_POLICY":"thread"},"owner_pubkey":owner.public_key()}}))
            .unwrap().save(sandbox_fake::client(&cluster),namespace).await.unwrap();
    }
    drop(second); // No owner's desktop is available to start the other agent.
    let config=ManagerConfig::parse(&json!({"version":1,"shared_tasks":true,"checkpoint_bucket":"fixture-checkpoints","developers":{
        "dev":{"owner_pubkey":a.public_key(),"namespace":NS,"profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"},
        "peer":{"owner_pubkey":b.public_key(),"namespace":"buzz-agent-staging-peer","profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/peer/profile-AbCdEf"}}}).to_string()).unwrap();
    let manager=Arc::new(Manager::new(keys("11"),config,sandbox_fake::client(&cluster),crate::enroll::tests::offline(Default::default()))
        .with_sandbox_defaults(json!({"image":format!("example.invalid/agent@sha256:{}","a".repeat(64)),"pod_options":{"active_deadline_seconds":3600}}).as_object().unwrap().clone()));
    let cancel = CancellationToken::new();
    let server = {
        let manager = manager.clone();
        let cancel = cancel.clone();
        let relay = relay.to_owned();
        tokio::spawn(async move {
            crate::serve::run_services(manager, &relay, None, true, cancel.cancelled()).await;
        })
    };
    wait_for(|| {
        cluster
            .lock()
            .unwrap()
            .secrets
            .values()
            .filter(|secret| secret["data"]["inbox.json"].is_string())
            .count()
            == 2
    })
    .await;
    // Discovery's activation floor is seconds, so a current message is included
    // by both first history and the overlapping live subscription.
    let first = publish(
        &mut phone,
        EventBuilder::new(Kind::Custom(9), "shared fixture task").tags([
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::public_key(agent_b.public_key()),
        ]),
        &a,
    )
    .await;
    wait_for(|| cluster.lock().unwrap().sandboxes.len() == 1).await;
    let b_dir = tempfile::Builder::new()
        .prefix("buzz-shared-b-")
        .tempdir()
        .unwrap()
        .keep();
    let a_dir = tempfile::Builder::new()
        .prefix("buzz-shared-a-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("Harness evidence: {} {}", a_dir.display(), b_dir.display());
    let b_process = harness(
        (acp, cli),
        &b_dir,
        relay,
        (&b, &agent_b),
        &first,
        Some(agent_a.public_key()),
    );
    let handoff = wait_message(
        &mut phone,
        channel,
        agent_b.public_key(),
        "shared fixture handoff",
    )
    .await;
    wait_for(|| cluster.lock().unwrap().sandboxes.len() == 2).await;
    let a_process = harness((acp, cli), &a_dir, relay, (&a, &agent_a), &first, None);
    let reply = wait_message(
        &mut phone,
        channel,
        agent_a.public_key(),
        "shared fixture completed",
    )
    .await;
    let thread_tags: Vec<_> = reply
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|t| {
            t.first().map(String::as_str) == Some("e")
                && matches!(t.get(3).map(String::as_str), Some("root" | "reply"))
        })
        .collect();
    assert!(!thread_tags.is_empty());
    assert!(thread_tags
        .iter()
        .all(|t| t.get(1) == Some(&first.id.to_hex())));
    assert_eq!(
        buzz_sdk::sharing::task::origin(&handoff)
            .unwrap()
            .unwrap()
            .request,
        first.id
    );
    assert_eq!(
        std::fs::read_to_string(a_dir.join("model-turns")).unwrap(),
        "1"
    );
    publish(
        &mut phone,
        buzz_sdk::sharing::Grant::builder(agent_a.public_key(), channel, false)
            .unwrap()
            .custom_created_at(Timestamp::from(Timestamp::now().as_secs() + 1)),
        &a,
    )
    .await;
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&b, &agent_b.public_key(), "").unwrap();
    let tag = buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap();
    let mut sender = NostrWsConnection::connect_authenticated(relay, &agent_b, Some(&tag))
        .await
        .unwrap();
    publish(
        &mut sender,
        buzz_sdk::sharing::task::handoff(
            &first,
            agent_b.public_key(),
            agent_a.public_key(),
            "denied after revoke",
            Timestamp::now().as_secs(),
        )
        .unwrap(),
        &agent_b,
    )
    .await;
    wait_message(&mut phone, channel, agent_a.public_key(), "refused").await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 2);
    assert_eq!(
        std::fs::read_to_string(a_dir.join("model-turns")).unwrap(),
        "1",
        "revocation must stop model access"
    );
    stop(a_process).await;
    stop(b_process).await;
    std::fs::remove_dir_all(a_dir).unwrap();
    std::fs::remove_dir_all(b_dir).unwrap();
    cancel.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}
