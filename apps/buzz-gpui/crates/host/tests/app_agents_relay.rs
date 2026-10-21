//! Agents2 custody against the TLS test relay, with a throwaway agent owned
//! by alice: create, publish its profile, post and read as the agent, write
//! memory, upload, rename, then delete and clean up.
//!
//! Skipped unless `BUZZ_TEST_RELAY_TLS` or `BUZZ_TEST_RELAY` names the test
//! relay's state directory (`.test-relay`) with the TLS relay up
//! (`scripts/test-relay.sh up-tls`). One test per binary: it sets
//! `SSL_CERT_FILE` before any other thread starts.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig};
use buzz_host_rpc::HostInfo;
use buzz_identity::{Identity, Purpose};
use buzz_relay_client::{Filter, RelayHttp, RelayOrigin};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn custody_against_the_test_relay() {
    let Some(dir) = std::env::var_os("BUZZ_TEST_RELAY_TLS")
        .or_else(|| std::env::var_os("BUZZ_TEST_RELAY"))
        .map(PathBuf::from)
    else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let read = |path: PathBuf| -> Value {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{}: {error}; run scripts/test-relay.sh up-tls",
                path.display()
            )
        });
        serde_json::from_str(&text).unwrap()
    };
    let identities = read(dir.join("identities.json"));
    let tls = read(dir.join("tls/relay.json"));
    // SAFETY: this binary's only test, before it starts any thread.
    unsafe { std::env::set_var("SSL_CERT_FILE", tls["ca_bundle"].as_str().unwrap()) };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let relay = tls["relay_ws_url"].as_str().unwrap().to_owned();
    let general = tls["channels"]["general"].as_str().unwrap().to_owned();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(lifecycle(alice, relay, general));
}

async fn ask(host: &AgentHost, request: AppAgentRequest) -> Result<AppAgentResponse, String> {
    match host.request(Request::AppAgent(request)).await {
        Ok(Response::AppAgent(response)) => Ok(response),
        Ok(other) => panic!("expected an app agent response, got {other:?}"),
        Err(error) => Err(error.to_string()),
    }
}

fn event(response: AppAgentResponse) -> Value {
    match response {
        AppAgentResponse::Event(json) => serde_json::from_str(&json).unwrap(),
        other => panic!("expected an event, got {other:?}"),
    }
}

async fn lifecycle(alice: Arc<Identity>, relay: String, general: String) {
    let state = tempfile::tempdir().unwrap();
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: "agents2".into(),
            name: "Agents2 test".into(),
            platform: std::env::consts::OS.into(),
            version: "test".into(),
        },
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: None,
    })
    .unwrap();

    let AppAgentResponse::Prepared { pubkey } = ask(
        &host,
        AppAgentRequest::CreatePrepare {
            request_id: "relay".into(),
            destination: relay.clone(),
            owner: alice.pubkey_hex(),
            agent_type: "test/echo".into(),
            name: "Agents2 Test".into(),
        },
    )
    .await
    .unwrap() else {
        panic!("expected prepared");
    };
    ask(
        &host,
        AppAgentRequest::CreateCommit {
            request_id: "relay".into(),
            auth: alice.attest_agent(&pubkey).unwrap(),
        },
    )
    .await
    .unwrap();

    // Its name becomes its profile, once.
    ask(
        &host,
        AppAgentRequest::PublishProfile {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .unwrap();
    let query = |filters: Value| AppAgentRequest::Query {
        pubkey: pubkey.clone(),
        filters: filters.to_string(),
    };
    let profiles = |response: AppAgentResponse| -> Vec<Value> {
        let AppAgentResponse::Events(json) = response else {
            panic!("expected events");
        };
        serde_json::from_str(&json).unwrap()
    };
    let read = profiles(
        ask(
            &host,
            query(json!([{ "kinds": [0], "authors": [pubkey], "limit": 1 }])),
        )
        .await
        .unwrap(),
    );
    let profile: Value = serde_json::from_str(read[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(profile, json!({ "name": "Agents2 Test", "bot": true }));

    // alice adds it to #general, where it posts and reads as itself.
    let http = RelayHttp::new(RelayOrigin::parse(&relay).unwrap(), alice.clone());
    let membership = |kind: u16| {
        let tags = vec![
            vec!["h".to_owned(), general.clone()],
            vec!["p".to_owned(), pubkey.clone()],
        ];
        let alice = alice.clone();
        let http = &http;
        async move {
            let unsigned = alice
                .template(Purpose::Content, kind, now(), tags, String::new())
                .unwrap();
            let signed = alice.sign(Purpose::Content, unsigned).unwrap();
            let receipt = http
                .post("/events", serde_json::to_string(&signed).unwrap())
                .await
                .unwrap();
            let receipt: Value = serde_json::from_slice(&receipt).unwrap();
            assert_eq!(receipt["accepted"], true, "kind {kind} refused: {receipt}");
        }
    };
    membership(9000).await;
    let content = format!("agents2 hello {}", now());
    let posted = event(
        ask(
            &host,
            AppAgentRequest::Publish {
                pubkey: pubkey.clone(),
                event: AppAgentEvent {
                    kind: 9,
                    content: content.clone(),
                    tags: vec![vec!["h".into(), general.clone()]],
                },
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(posted["pubkey"], pubkey);
    assert!(
        posted["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag[0] == "auth"),
        "the owner attestation is attached"
    );
    let mine = profiles(
        ask(
            &host,
            query(json!([{ "kinds": [9], "authors": [pubkey], "#h": [general], "limit": 5 }])),
        )
        .await
        .unwrap(),
    );
    assert!(mine.iter().any(|e| e["content"] == content), "{mine:?}");

    // Memory is encrypted to alice, who reads it back.
    let remembered = event(
        ask(
            &host,
            AppAgentRequest::Remember {
                pubkey: pubkey.clone(),
                slug: "mem/notes".into(),
                body: "Remember the milk".into(),
                after: 0,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(remembered["kind"], 30174);
    let mut filter = Filter::new();
    filter.insert("kinds".into(), json!([30174]));
    filter.insert("authors".into(), json!([pubkey]));
    filter.insert("#p".into(), json!([alice.pubkey_hex()]));
    let records: Vec<Option<Value>> = http
        .query_raw(&[filter])
        .await
        .unwrap()
        .into_iter()
        .map(Some)
        .collect();
    let memories = alice.decode_agent_memories(&pubkey, &records).unwrap();
    assert!(
        memories.to_string().contains("Remember the milk"),
        "{memories}"
    );

    // Media goes up as the agent.
    match ask(
        &host,
        AppAgentRequest::Upload {
            pubkey: pubkey.clone(),
            data: PNG.into(),
            mime: "image/png".into(),
            staged: None,
        },
    )
    .await
    {
        Ok(AppAgentResponse::Uploaded(json)) => {
            let receipt: Value = serde_json::from_str(&json).unwrap();
            assert!(receipt["url"].is_string(), "{receipt}");
        }
        other => panic!("upload: {other:?}"),
    }

    // A rename is published as a newer profile.
    ask(
        &host,
        AppAgentRequest::Rename {
            pubkey: pubkey.clone(),
            name: "Agents2 Renamed".into(),
        },
    )
    .await
    .unwrap();
    ask(
        &host,
        AppAgentRequest::PublishProfile {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = profiles(
            ask(
                &host,
                query(json!([{ "kinds": [0], "authors": [pubkey], "limit": 1 }])),
            )
            .await
            .unwrap(),
        );
        if read[0]["content"]
            .as_str()
            .unwrap()
            .contains("Agents2 Renamed")
        {
            break;
        }
        assert!(Instant::now() < deadline, "the rename was not published");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Delete: the key goes at once; the owner cleans up the community, then
    // the identity is forgotten.
    ask(
        &host,
        AppAgentRequest::Delete {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .unwrap();
    let refused = ask(&host, query(json!([{ "kinds": [0], "limit": 1 }]))).await;
    assert_eq!(refused.unwrap_err(), "No such agent on this device");
    membership(9001).await;
    ask(
        &host,
        AppAgentRequest::Forget {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .unwrap();
    let AppAgentResponse::Agents(left) = ask(&host, AppAgentRequest::List).await.unwrap() else {
        panic!("expected agents");
    };
    assert!(left.is_empty());
}
