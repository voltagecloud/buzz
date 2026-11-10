//! Real HTTP/WS and spawned IPC bind the production admission path.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use nostr::{EventBuilder, Kind};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::*;

struct Fixture {
    config: Config,
    rest: RestClient,
    events: Arc<Mutex<Vec<Event>>>,
    request: Event,
    approval: Event,
    owner: Keys,
    relay: Keys,
    requester: Keys,
    directory: tempfile::TempDir,
    http: tokio::task::JoinHandle<()>,
}

async fn query(
    State(events): State<Arc<Mutex<Vec<Event>>>>,
    Json(filters): Json<Vec<Value>>,
) -> Json<Value> {
    let filter = &filters[0];
    assert_eq!(
        filter["consistency"], "strong",
        "all authority/head reads must use writer consistency"
    );
    let mut result: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            let includes = |key, value| {
                filter[key]
                    .as_array()
                    .is_none_or(|items| items.contains(&value))
            };
            includes("kinds", json!(event.kind.as_u16()))
                && includes("authors", json!(event.pubkey))
                && includes("ids", json!(event.id))
                && filter["#d"].as_array().is_none_or(|items| {
                    event.tags.iter().any(|tag| {
                        let values = tag.as_slice();
                        values.first().is_some_and(|key| key == "d")
                            && values
                                .get(1)
                                .is_some_and(|value| items.contains(&json!(value)))
                    })
                })
        })
        .cloned()
        .collect();
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    result.truncate(filter["limit"].as_u64().unwrap_or(256) as usize);
    Json(json!(result))
}

impl Fixture {
    async fn new(gateway: bool) -> Self {
        let (owner, agent, relay, requester) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
            Keys::generate(),
        );
        let requester = if gateway { requester } else { owner.clone() };
        let scope = Scope {
            channel: Uuid::new_v4(),
            thread: EventId::all_zeros(),
            agent: agent.public_key(),
        };
        let now = Timestamp::now().as_secs();
        let request = attach::request(&requester, scope, Uuid::new_v4(), now + 600).unwrap();
        let pending = attach::Pending::parse(&request, now).unwrap();
        let approval = attach::approve(&owner, &pending, now + 600).unwrap();
        let tags = std::iter::once(Tag::identifier(scope.channel.to_string())).chain(
            [
                owner.public_key(),
                agent.public_key(),
                requester.public_key(),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|key| Tag::parse(["p", &key.to_hex(), "", "member"]).unwrap()),
        );
        let roster = EventBuilder::new(Kind::Custom(39002), "")
            .tags(tags)
            .sign_with_keys(&relay)
            .unwrap();
        let mut events = vec![request.clone(), approval.clone(), roster];
        if gateway {
            events.push(
                buzz_sdk::gateway::Delegation::grant(requester.public_key(), now + 600, now)
                    .unwrap()
                    .sign_with_keys(&owner)
                    .unwrap(),
            );
            events.push(
                EventBuilder::new(Kind::Metadata, r#"{"name":"gateway"}"#)
                    .sign_with_keys(&requester)
                    .unwrap(),
            );
        }
        let events = Arc::new(Mutex::new(events));
        let relay_key = relay.public_key();
        let app = Router::new()
            .route(
                "/",
                get(move || async move { Json(json!({"self":relay_key})) }),
            )
            .route("/query", post(query))
            .with_state(events.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("worker");
        std::fs::write(
            &executable,
            r#"#!/usr/bin/env python3
import os, sys, signal
os.chdir(sys.argv[2])
with open('marker', 'w') as marker: marker.write('spawned')
def end(*_):
    print('{"type":"ended","failed":false}', flush=True)
    sys.exit(0)
signal.signal(signal.SIGTERM, end)
sys.stdin.readline()
print('{"type":"ready"}', flush=True)
for _ in sys.stdin: pass
end()
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            config: Config {
                relay: String::new(),
                keys: agent.clone(),
                auth: None,
                owner: owner.public_key(),
                scope,
                workspace: directory.path().to_owned(),
                executable,
                journal: directory.path().join("journal"),
            },
            rest: RestClient {
                http: reqwest::Client::new(),
                base_url,
                keys: agent,
                auth_tag_json: None,
            },
            events,
            request,
            approval,
            owner,
            relay,
            requester,
            directory,
            http,
        }
    }
    fn open_event(&self) -> Event {
        attach::encrypt(
            &self.requester,
            self.config.scope.agent,
            self.config.scope,
            &Frame {
                grant: self.approval.id,
                seq: 1,
                ack: 0,
                packet: Packet::Open { rows: 37, cols: 91 },
            },
        )
        .unwrap()
    }
    fn journal(&self) -> Journal {
        Journal::open(
            self.config.journal.clone(),
            self.config.scope,
            self.config.owner,
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.http.abort();
    }
}

async fn relay_receipt(message: &str) -> (NostrWsConnection, tokio::task::JoinHandle<()>) {
    relay_receipt_after(message, Duration::ZERO).await
}

async fn relay_receipt_after(
    message: &str,
    delay: Duration,
) -> (NostrWsConnection, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let message = message.to_owned();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value[0] == "EVENT" {
                tokio::time::sleep(delay).await;
                ws.send(Message::Text(
                    json!(["OK", value[1]["id"], true, message])
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        }
    });
    (NostrWsConnection::connect(&url).await.unwrap(), task)
}

#[tokio::test]
async fn delayed_claim_receipt_cannot_start_a_worker_after_admission_expiry() {
    let mut f = Fixture::new(false).await;
    let pending = attach::Pending::parse(&f.request, Timestamp::now().as_secs()).unwrap();
    let approval = attach::approve(&f.owner, &pending, Timestamp::now().as_secs() + 3).unwrap();
    f.events
        .lock()
        .unwrap()
        .retain(|event| event.id != f.approval.id);
    f.events.lock().unwrap().push(approval.clone());
    f.approval = approval;
    let journal = f.journal();
    let (mut connection, task) =
        relay_receipt_after(attach::CLAIM_OK_MESSAGE, Duration::from_secs(4)).await;
    let result = open(
        &mut connection,
        &f.config,
        &f.rest,
        &journal,
        &f.open_event(),
        &mut Rate::default(),
        Instant::now(),
    )
    .await;
    let mut active = result.ok();
    finish(&f.config, &journal, &mut active).await.unwrap();
    assert!(
        !f.directory.path().join("marker").exists(),
        "expired admission spawned a worker before its lease check"
    );
    let record = journal.records().unwrap().remove(0);
    assert!(
        record.claimed && record.ended.is_some(),
        "expired owned claim was not durably ended"
    );
    task.abort();
}

#[tokio::test]
async fn failed_audit_write_cannot_downgrade_unconfirmed_startup_containment() {
    let f = Fixture::new(false).await;
    std::fs::write(
        &f.config.executable,
        r#"#!/usr/bin/env python3
import os, sys
os.chdir(sys.argv[2])
os.rename('journal', 'retained-journal')
sys.exit(2)
"#,
    )
    .unwrap();
    let journal = f.journal();
    let (mut connection, task) = relay_receipt(attach::CLAIM_OK_MESSAGE).await;
    let result = open(
        &mut connection,
        &f.config,
        &f.rest,
        &journal,
        &f.open_event(),
        &mut Rate::default(),
        Instant::now(),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(mut session) => {
            session.pty.close().await.unwrap();
            panic!("uncontained worker unexpectedly started")
        }
    };
    assert!(
        error.is::<client::ContainmentError>(),
        "journal failure hid fatal startup containment: {error}"
    );
    assert!(
        f.directory
            .path()
            .join("retained-journal")
            .join(format!("{}.json", f.approval.id))
            .exists(),
        "durable owned acceptance was lost"
    );
    task.abort();
}

#[tokio::test]
async fn open_requires_the_atomic_first_claim_receipt_before_worker_start() {
    for message in [
        "saved",
        "duplicate: already have this event",
        attach::CLAIM_OK_MESSAGE,
    ] {
        let f = Fixture::new(false).await;
        let journal = f.journal();
        let (mut connection, task) = relay_receipt(message).await;
        let result = open(
            &mut connection,
            &f.config,
            &f.rest,
            &journal,
            &f.open_event(),
            &mut Rate::default(),
            Instant::now(),
        )
        .await;
        let accepted = message == attach::CLAIM_OK_MESSAGE;
        let did_open = result.is_ok();
        let mut active = result.ok();
        finish(&f.config, &journal, &mut active).await.unwrap();
        assert_eq!(did_open, accepted);
        assert_eq!(
            f.directory.path().join("marker").exists(),
            accepted,
            "old/generic/duplicate receipt started a worker"
        );
        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].claimed, accepted);
        assert_eq!(
            records[0].ended.is_some(),
            accepted,
            "a losing claim falsely ended another replica's operation"
        );
        // A restart/replayed Open never reuses this owner approval locally.
        let grant = Grant::parse(&f.approval, f.config.scope, Timestamp::now().as_secs()).unwrap();
        assert!(journal.reserve(&f.config.keys, &grant).is_err());
        task.abort();
    }
}

#[tokio::test]
async fn current_gateway_approval_requires_all_strong_heads_and_posting_membership() {
    let f = Fixture::new(true).await;
    let check = || {
        authority::approve(
            &f.rest,
            f.config.owner,
            f.config.scope,
            f.approval.id,
            f.requester.public_key(),
        )
    };
    let grant = check().await.unwrap();
    let baseline = f.events.lock().unwrap().clone();
    f.events
        .lock()
        .unwrap()
        .iter_mut()
        .find(|event| event.id == f.approval.id)
        .unwrap()
        .content
        .push(' ');
    assert!(
        authority::refresh(&f.rest, f.config.owner, &grant)
            .await
            .is_err(),
        "malformed current approval bypassed refresh validation"
    );
    *f.events.lock().unwrap() = baseline.clone();
    for remove in [f.request.id, f.approval.id] {
        f.events.lock().unwrap().retain(|event| event.id != remove);
        assert!(
            check().await.is_err(),
            "deleted current request/grant retained shell authority"
        );
        *f.events.lock().unwrap() = baseline.clone();
    }
    f.events
        .lock()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 30181);
    assert!(
        check().await.is_err(),
        "revoked gateway retained shell authority"
    );
    *f.events.lock().unwrap() = baseline.clone();
    let roster = baseline
        .iter()
        .find(|event| event.kind.as_u16() == 39002)
        .unwrap();
    for missing in [
        f.config.owner,
        f.config.scope.agent,
        f.requester.public_key(),
    ] {
        let key = missing.to_hex();
        let tags = roster
            .tags
            .iter()
            .filter(|tag| tag.as_slice().get(1) != Some(&key))
            .cloned();
        let removed = EventBuilder::new(roster.kind, "")
            .tags(tags)
            .sign_with_keys(&f.relay)
            .unwrap();
        f.events
            .lock()
            .unwrap()
            .retain(|event| event.kind.as_u16() != 39002);
        f.events.lock().unwrap().push(removed);
        assert!(
            check().await.is_err(),
            "loss of posting membership retained terminal authority"
        );
        *f.events.lock().unwrap() = baseline.clone();
    }
    let tags = roster
        .tags
        .iter()
        .filter(|tag| tag.as_slice().get(1) != Some(&f.requester.public_key().to_hex()))
        .cloned();
    // A spoofed roster with the right entries cannot replace relay authority.
    let spoofed = EventBuilder::new(roster.kind, "")
        .tags(tags)
        .sign_with_keys(&f.owner)
        .unwrap();
    f.events
        .lock()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 39002);
    f.events.lock().unwrap().push(spoofed);
    assert!(check().await.is_err());
}

#[tokio::test]
async fn journal_recovers_owned_audit_without_reopening_or_ending_a_losing_claim() {
    let f = Fixture::new(false).await;
    let grant = Grant::parse(&f.approval, f.config.scope, Timestamp::now().as_secs()).unwrap();
    let journal = f.journal();
    assert!(
        Journal::open(f.config.journal.clone(), f.config.scope, f.config.owner).is_err(),
        "two local servers acquired the same journal"
    );
    let record = journal.reserve(&f.config.keys, &grant).unwrap();
    let accepted = record.accepted.id;
    drop(journal);
    let journal = f.journal();
    journal.recover(&f.config.keys).unwrap();
    assert!(journal.reserve(&f.config.keys, &grant).is_err());
    let mut record = journal.records().unwrap().remove(0);
    assert_eq!(record.accepted.id, accepted);
    assert!(
        record.ended.is_none(),
        "unconfirmed receipt falsely ended a different replica's terminal"
    );
    record.claimed = true;
    record.bytes_in = 12;
    record.bytes_out = 34;
    journal.save(&record).unwrap();
    journal.recover(&f.config.keys).unwrap();
    let record = journal.records().unwrap().remove(0);
    let ended = record.ended.unwrap();
    let body: Value = serde_json::from_str(&ended.content).unwrap();
    assert_eq!(body["bytes_in"], 12);
    assert_eq!(body["bytes_out"], 34);
    assert_eq!(body["grant"]["id"], json!(grant.id()));
    journal.recover(&f.config.keys).unwrap();
    assert_eq!(
        journal.records().unwrap()[0].ended.as_ref().unwrap().id,
        ended.id,
        "audit retries regenerated their signed event"
    );
    assert!(journal.reserve(&f.config.keys, &grant).is_err());
}

#[test]
fn production_factory_refuses_unbound_dm_non_owner_and_plaintext_transport() {
    let mut config = crate::build_mcp_servers_tests::test_config();
    let owner = Keys::generate();
    config.agent_owner = Some(owner.public_key().to_hex());
    config.relay_url = "wss://relay.example.test".into();
    config.respond_to = RespondTo::Owner;
    config.bound_session = Some(BoundSession::Thread {
        channel_id: Uuid::new_v4(),
        thread_root: EventId::all_zeros().to_hex(),
    });
    assert!(Config::from_harness(&config).unwrap().is_some());
    config.relay_url = "ws://relay.example.test".into();
    assert!(Config::from_harness(&config).unwrap().is_none());
    config.relay_url = "wss://relay.example.test".into();
    config.respond_to = RespondTo::Anyone;
    assert!(Config::from_harness(&config).unwrap().is_none());
    config.respond_to = RespondTo::Owner;
    config.bound_session = Some(BoundSession::DirectMessage {
        dm_channel_id: Uuid::new_v4(),
    });
    assert!(Config::from_harness(&config).unwrap().is_none());
    config.bound_session = None;
    assert!(Config::from_harness(&config).unwrap().is_none());
}

#[tokio::test]
async fn harness_shutdown_propagates_unconfirmed_containment_and_task_panics() {
    let ordinary = tokio::spawn(async { Err(anyhow::anyhow!("transport unavailable")) });
    assert!(crate::sandbox_terminal::stop(Some(ordinary)).await.is_ok());
    let fatal = tokio::spawn(async { Err(client::ContainmentError.into()) });
    let error = crate::sandbox_terminal::stop(Some(fatal))
        .await
        .unwrap_err();
    assert!(crate::sandbox_terminal::containment_failure(&error));
    let panic: tokio::task::JoinHandle<Result<()>> =
        tokio::spawn(async { panic!("deliberate terminal task panic") });
    assert!(crate::sandbox_terminal::stop(Some(panic)).await.is_err());
}
