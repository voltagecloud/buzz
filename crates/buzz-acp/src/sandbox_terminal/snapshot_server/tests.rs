//! Real HTTP/WS, signed metadata and child IPC exercise the production export seam.
use super::*;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use nostr::hashes::{sha256, Hash};
use nostr::{EventBuilder, EventId, Keys, Kind, Tag};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Success,
    NoIntentHead,
    RefuseIntent,
    RevokeAfterPut,
    AmbiguousPut,
    SlowIntentRevocation,
    TransientPreclaim,
}
struct StateData {
    events: Vec<Event>,
    uploads: usize,
    approval: EventId,
    mode: Mode,
    ciphertext: Vec<u8>,
    deny_authority: bool,
    authority_failures: usize,
    record_path: Option<PathBuf>,
}
type Shared = Arc<Mutex<StateData>>;
async fn query(State(state): State<Shared>, Json(filters): Json<Vec<Value>>) -> Json<Value> {
    let filter = &filters[0];
    assert_eq!(filter["consistency"], "strong");
    {
        let mut state = state.lock().unwrap();
        if state.deny_authority && filter["kinds"] == json!([30181]) {
            state.authority_failures += 1;
            return Json(json!({"error":"temporary authority read failure"}));
        }
    }
    let slow = state.lock().unwrap().mode == Mode::SlowIntentRevocation
        && filter["#d"].as_array().is_some_and(|items| {
            items.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|value| value.ends_with(":upload"))
            })
        });
    if slow {
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    let state = state.lock().unwrap();
    let mut result: Vec<_> = state
        .events
        .iter()
        .filter(|event| {
            let has = |key, value| {
                filter[key]
                    .as_array()
                    .is_none_or(|items| items.contains(&value))
            };
            has("kinds", json!(event.kind.as_u16()))
                && has("authors", json!(event.pubkey))
                && has("ids", json!(event.id))
                && ["d", "h", "e"].into_iter().all(|key| {
                    filter[format!("#{key}")].as_array().is_none_or(|items| {
                        event.tags.iter().any(|tag| {
                            let values = tag.as_slice();
                            values.first().is_some_and(|value| value == key)
                                && values
                                    .get(1)
                                    .is_some_and(|value| items.contains(&json!(value)))
                        })
                    })
                })
        })
        .cloned()
        .collect();
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    result.truncate(filter["limit"].as_u64().unwrap_or(256) as usize);
    Json(json!(result))
}
async fn upload(State(state): State<Shared>, body: Bytes) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.uploads += 1;
    state.ciphertext = body.to_vec();
    if state.mode == Mode::RevokeAfterPut {
        let id = state.approval;
        state.events.retain(|event| event.id != id);
    }
    if state.mode == Mode::AmbiguousPut {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"private":"never forward"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"sha256":sha256::Hash::hash(&body).to_string(),"size":body.len()})),
    )
}
struct Fixture {
    config: Config,
    rest: RestClient,
    grant: Grant,
    owner: Keys,
    state: Shared,
    directory: tempfile::TempDir,
    http: tokio::task::JoinHandle<()>,
    ws: tokio::task::JoinHandle<()>,
    connection: NostrWsConnection,
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        let (owner, agent, relay) = (Keys::generate(), Keys::generate(), Keys::generate());
        let scope = attach::Scope {
            channel: Uuid::new_v4(),
            thread: EventId::all_zeros(),
            agent: agent.public_key(),
        };
        let now = Timestamp::now().as_secs();
        let request = snapshot::request(&owner, scope, Uuid::new_v4(), now + 600).unwrap();
        let pending = snapshot::Pending::parse(&request, now).unwrap();
        let approval = snapshot::approve(&owner, &pending, now + 600).unwrap();
        let grant = Grant::parse(&approval, scope, now).unwrap();
        let roster = EventBuilder::new(Kind::Custom(39002), "")
            .tag(Tag::identifier(scope.channel.to_string()))
            .tags(
                [owner.public_key(), agent.public_key()]
                    .into_iter()
                    .map(|key| Tag::parse(["p", &key.to_hex(), "", "member"]).unwrap()),
            )
            .sign_with_keys(&relay)
            .unwrap();
        let state = Arc::new(Mutex::new(StateData {
            events: vec![request, approval.clone(), roster],
            uploads: 0,
            approval: approval.id,
            mode,
            ciphertext: Vec::new(),
            deny_authority: mode == Mode::TransientPreclaim,
            authority_failures: 0,
            record_path: None,
        }));
        let relay_key = relay.public_key();
        let app = Router::new()
            .route(
                "/",
                get(move || async move { Json(json!({"self":relay_key})) }),
            )
            .route("/query", post(query))
            .route("/upload", put(upload))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let shared = state.clone();
        let ws = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value[0] != "EVENT" {
                    continue;
                }
                let event: Event = serde_json::from_value(value[1].clone()).unwrap();
                assert!(event.verify().is_ok());
                let is_intent = event.tags.iter().any(|tag| {
                    tag.as_slice()
                        .get(1)
                        .is_some_and(|value| value.ends_with(":upload"))
                });
                if is_intent {
                    assert_saved_intent(&shared, &event);
                }
                let (accepted, slow) = {
                    let mut state = shared.lock().unwrap();
                    let accepted = !(is_intent && state.mode == Mode::RefuseIntent);
                    if accepted && !(is_intent && state.mode == Mode::NoIntentHead) {
                        state.events.push(event.clone());
                    }
                    let slow = is_intent && state.mode == Mode::SlowIntentRevocation;
                    if slow {
                        let approval = state.approval;
                        state.events.retain(|event| event.id != approval);
                    }
                    (accepted, slow)
                };
                if slow {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }

                ws.send(Message::Text(
                    json!(["OK", event.id, accepted, "saved"])
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let connection = NostrWsConnection::connect(&url).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        state.lock().unwrap().record_path = Some(
            directory
                .path()
                .join("journal")
                .join(format!("{}.json", grant.id())),
        );
        let executable = directory.path().join("worker");
        std::fs::write(&executable,r#"#!/usr/bin/env python3
import json,os,sys
out=sys.argv[3]
fd=os.open(out,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'wb') as f:f.write(b'fixture scanned archive')
print('{"type":"started"}',flush=True)
print(json.dumps({'type':'ended','receipt':{'version':1,'scanned':True,'repositories':1,'bytes':23},'reason':None}),flush=True)
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            config: Config {
                relay: url,
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
            grant,
            owner,
            state,
            directory,
            http,
            ws,
            connection,
        }
    }
    async fn run(&mut self) -> Record {
        let journal = Journal::open(
            self.config.journal.clone(),
            self.config.scope,
            self.config.owner,
        )
        .unwrap();
        let mut record = journal.reserve(&self.config.keys, &self.grant).unwrap();
        record.claimed = true;
        journal.save(&record).unwrap();
        let (_shutdown, rx) = watch::channel(());
        export(
            &self.config,
            &self.rest,
            &mut self.connection,
            &journal,
            &self.grant,
            &mut record,
            PathBuf::from("/unused-test-helper"),
            rx,
        )
        .await
        .unwrap();
        record
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.http.abort();
        self.ws.abort();
    }
}
fn phase(event: &Event, suffix: &str) -> bool {
    event.tags.iter().any(|tag| {
        tag.as_slice().first().is_some_and(|key| key == "d")
            && tag
                .as_slice()
                .get(1)
                .is_some_and(|value| value.ends_with(suffix))
    })
}

#[tokio::test]
async fn missing_or_ambiguous_intent_confirmation_prevents_any_put() {
    for mode in [Mode::NoIntentHead, Mode::RefuseIntent] {
        let mut fixture = Fixture::new(mode).await;
        let record = fixture.run().await;
        assert_eq!(fixture.state.lock().unwrap().uploads, 0);
        assert_eq!(
            record.metadata.len(),
            1,
            "failed relay confirmation must retain exact local retry intent"
        );
        assert!(record.sent.is_empty());
        assert!(record.ended.is_some());
        assert!(!fixture
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|event| phase(event, ":ready")));
    }
}
#[tokio::test]
async fn authority_loss_after_upload_prevents_key_delivery() {
    let mut fixture = Fixture::new(Mode::RevokeAfterPut).await;
    let record = fixture.run().await;
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.uploads, 1);
    assert!(state.events.iter().any(|event| phase(event, ":upload")));
    assert!(!state.events.iter().any(|event| phase(event, ":ready")));
    assert!(matches!(
        snapshot::End::parse(record.ended.as_ref().unwrap(), &fixture.grant)
            .unwrap()
            .reason(),
        Some(Failure::Authority)
    ));
}
#[tokio::test]
async fn ambiguous_upload_keeps_recoverable_intent_without_retry_or_key() {
    let mut fixture = Fixture::new(Mode::AmbiguousPut).await;
    let record = fixture.run().await;
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.uploads, 1);
    let intent = state
        .events
        .iter()
        .find(|event| phase(event, ":upload"))
        .unwrap();
    assert!(
        snapshot::upload::Upload::parse(intent, fixture.config.scope, fixture.config.owner).is_ok()
    );
    let outcome = state
        .events
        .iter()
        .find(|event| phase(event, ":upload-outcome"))
        .unwrap();
    assert_eq!(
        snapshot::upload::Upload::parse(outcome, fixture.config.scope, fixture.config.owner)
            .unwrap()
            .state(),
        snapshot::upload::State::Unconfirmed
    );
    assert!(!state.events.iter().any(|event| phase(event, ":ready")));
    assert!(matches!(
        snapshot::End::parse(record.ended.as_ref().unwrap(), &fixture.grant)
            .unwrap()
            .reason(),
        Some(Failure::Upload)
    ));
}
#[tokio::test]
async fn successful_export_delivers_exact_ciphertext_only_after_durable_intent() {
    let mut fixture = Fixture::new(Mode::Success).await;
    let record = fixture.run().await;
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.uploads, 1);
    let ready = state
        .events
        .iter()
        .find(|event| phase(event, ":ready"))
        .unwrap();
    let receipt = snapshot::receipt(
        &fixture.owner,
        &fixture.grant,
        ready,
        Timestamp::now().as_secs(),
    )
    .unwrap();
    assert_eq!(
        receipt
            .open(&state.ciphertext, Timestamp::now().as_secs())
            .unwrap(),
        b"fixture scanned archive"
    );
    assert!(record.metadata.iter().all(|event| !phase(event, ":ready")));
    assert!(
        snapshot::End::parse(record.ended.as_ref().unwrap(), &fixture.grant)
            .unwrap()
            .reason()
            .is_none()
    );
    assert!(!fixture
        .directory
        .path()
        .join("journal")
        .join(format!("{}.job", fixture.grant.id()))
        .exists());
}

#[tokio::test]
async fn stale_keyless_outbox_confirms_existing_heads_or_persists_fresh_envelopes() {
    for future in [false, true] {
        let mut fixture = Fixture::new(Mode::Success).await;
        let record = fixture.run().await;
        let original = record.ended.as_ref().unwrap();
        let old = EventBuilder::new(original.kind, original.content.clone())
            .tags(original.tags.clone().to_vec())
            .custom_created_at(Timestamp::from(if future {
                Timestamp::now().as_secs() + 1000
            } else {
                Timestamp::now().as_secs() - 1000
            }))
            .sign_with_keys(&fixture.config.keys)
            .unwrap();
        let journal = Journal::open(
            fixture.config.journal.clone(),
            fixture.config.scope,
            fixture.config.owner,
        )
        .unwrap();
        let mut record = record;
        record.ended = Some(old.clone());
        record.ended_sent = false;
        journal.save(&record).unwrap();
        outbox(
            &mut fixture.connection,
            &fixture.rest,
            &journal,
            &fixture.config.keys,
        )
        .await
        .unwrap();
        let saved = journal.records().unwrap().pop().unwrap();
        let fresh = saved.ended.as_ref().unwrap();
        assert_ne!(fresh.id, old.id);
        assert_eq!(fresh.content, old.content);
        assert_eq!(fresh.tags, old.tags);
        assert!(saved.ended_sent);
        assert!(
            Timestamp::now()
                .as_secs()
                .abs_diff(fresh.created_at.as_secs())
                < 60
        );
        let writes = fixture.state.lock().unwrap().events.len();
        let mut record = saved;
        record.ended_sent = false;
        journal.save(&record).unwrap();
        outbox(
            &mut fixture.connection,
            &fixture.rest,
            &journal,
            &fixture.config.keys,
        )
        .await
        .unwrap();
        assert_eq!(
            fixture.state.lock().unwrap().events.len(),
            writes,
            "confirmed stored head must not be resubmitted"
        );
    }
}

#[tokio::test]
async fn revocation_monitor_stays_live_while_metadata_confirmation_waits() {
    let mut fixture = Fixture::new(Mode::SlowIntentRevocation).await;
    let record = fixture.run().await;
    assert_eq!(
        fixture.state.lock().unwrap().uploads,
        0,
        "blocked keyless metadata must not hide authority loss before PUT"
    );
    assert!(matches!(
        snapshot::End::parse(record.ended.as_ref().unwrap(), &fixture.grant)
            .unwrap()
            .reason(),
        Some(Failure::Authority)
    ));
}

#[test]
fn generic_duplicate_or_wrong_attempt_receipts_never_grant_export_authority() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(30624), "")
        .sign_with_keys(&keys)
        .unwrap();
    for message in ["saved", "duplicate:", ""] {
        assert!(first_claim(
            &OkResponse {
                event_id: event.id.to_hex(),
                accepted: true,
                message: message.into()
            },
            &event
        )
        .is_err());
    }
    assert!(first_claim(
        &OkResponse {
            event_id: event.id.to_hex(),
            accepted: true,
            message: attach::CLAIM_OK_MESSAGE.into()
        },
        &event
    )
    .is_ok());
    assert!(first_claim(
        &OkResponse {
            event_id: EventId::all_zeros().to_hex(),
            accepted: true,
            message: attach::CLAIM_OK_MESSAGE.into()
        },
        &event
    )
    .is_err());
}

#[tokio::test]
async fn failed_strong_head_read_never_resigns_or_changes_durable_outbox() {
    let mut fixture = Fixture::new(Mode::Success).await;
    let mut record = fixture.run().await;
    let original = record.ended.as_ref().unwrap();
    let old = EventBuilder::new(original.kind, original.content.clone())
        .tags(original.tags.clone().to_vec())
        .custom_created_at(Timestamp::from(Timestamp::now().as_secs() - 1000))
        .sign_with_keys(&fixture.config.keys)
        .unwrap();
    record.ended = Some(old.clone());
    record.ended_sent = false;
    let journal = Journal::open(
        fixture.config.journal.clone(),
        fixture.config.scope,
        fixture.config.owner,
    )
    .unwrap();
    journal.save(&record).unwrap();
    let mut failed = fixture.rest.clone();
    failed.base_url = "http://127.0.0.1:1".into();
    assert!(outbox(
        &mut fixture.connection,
        &failed,
        &journal,
        &fixture.config.keys
    )
    .await
    .is_err());
    assert_eq!(
        journal
            .records()
            .unwrap()
            .pop()
            .unwrap()
            .ended
            .as_ref()
            .unwrap(),
        &old
    );
}

fn assert_saved_intent(state: &Shared, event: &Event) {
    let state = state.lock().unwrap();
    let path = state.record_path.as_ref().unwrap();
    let record: Record = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(
        record.claimed && record.metadata.iter().any(|saved| saved == event),
        "upload intent reached relay before exact durable local save"
    );
}

#[cfg(test)]
mod service;
