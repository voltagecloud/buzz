//! Agents2 custody over host-rpc, and how a host without it answers.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, EventSource, HostClient, HostClientOptions, HostHandler,
    HostInfo, Peer, Protocol, RpcError, ServerOptions, serve, transport,
};
use buzz_identity::Identity;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

struct NoAsks;

impl ClientHandler<HostProtocol> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

fn info() -> HostInfo {
    HostInfo {
        id: "laptop".into(),
        name: "This laptop".into(),
        platform: "linux".into(),
        version: "test".into(),
    }
}

async fn connected(client: &HostClient<HostProtocol>) {
    client
        .state()
        .wait_for(|s| matches!(s, ConnectionState::Connected { .. }))
        .await
        .unwrap();
}

async fn ask(
    client: &HostClient<HostProtocol>,
    request: AppAgentRequest,
) -> Result<AppAgentResponse, RpcError> {
    match client.request(Request::AppAgent(request)).await? {
        Response::AppAgent(response) => Ok(response),
        other => panic!("expected an app agent response, got {other:?}"),
    }
}

async fn list(client: &HostClient<HostProtocol>) -> Vec<AppAgentSummary> {
    match ask(client, AppAgentRequest::List).await.unwrap() {
        AppAgentResponse::Agents(agents) => agents,
        other => panic!("expected agents, got {other:?}"),
    }
}

fn failed(result: Result<AppAgentResponse, RpcError>) -> String {
    match result {
        Err(RpcError::Failed(message)) => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn custody_over_rpc() {
    let state = tempfile::tempdir().unwrap();
    let host = AgentHost::start(HostConfig {
        info: info(),
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    connected(&client).await;
    assert!(list(&client).await.is_empty());

    let owner = Identity::ephemeral();
    let prepare = |name: &str| AppAgentRequest::CreatePrepare {
        request_id: "create-1".into(),
        destination: "wss://Relay.Example.com/".into(),
        owner: owner.pubkey_hex(),
        agent_type: "test/echo".into(),
        name: name.into(),
    };
    let AppAgentResponse::Prepared { pubkey } = ask(&client, prepare("Ada")).await.unwrap() else {
        panic!("expected prepared");
    };
    let AppAgentResponse::Prepared { pubkey: again } = ask(&client, prepare("Ada")).await.unwrap()
    else {
        panic!("expected prepared");
    };
    assert_eq!(pubkey, again, "a retried prepare reuses the key");
    assert_eq!(
        failed(ask(&client, prepare("Grace")).await),
        "Create destination or owner changed"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::CreatePrepare {
                    request_id: "create-2".into(),
                    destination: "wss://relay.example.com".into(),
                    owner: owner.pubkey_hex(),
                    agent_type: "test/echo".into(),
                    name: "two\nlines".into(),
                },
            )
            .await
        ),
        "Name the agent in one line"
    );

    // An attestation by someone else is refused, and the key is gone with it.
    let stranger = Identity::ephemeral();
    let refused = ask(
        &client,
        AppAgentRequest::CreateCommit {
            request_id: "create-1".into(),
            auth: stranger.attest_agent(&pubkey).unwrap(),
        },
    )
    .await;
    assert!(matches!(refused, Err(RpcError::Failed(_))), "{refused:?}");
    assert!(list(&client).await.is_empty(), "nothing is saved");

    let AppAgentResponse::Prepared { pubkey } = ask(&client, prepare("Ada")).await.unwrap() else {
        panic!("expected prepared");
    };
    let AppAgentResponse::Created(created) = ask(
        &client,
        AppAgentRequest::CreateCommit {
            request_id: "create-1".into(),
            auth: owner.attest_agent(&pubkey).unwrap(),
        },
    )
    .await
    .unwrap() else {
        panic!("expected created");
    };
    assert_eq!(
        created,
        AppAgentSummary {
            pubkey: pubkey.clone(),
            relay: "wss://relay.example.com".into(),
            owner: owner.pubkey_hex(),
            agent_type: "test/echo".into(),
            name: "Ada".into(),
            picture: Some(String::new()),
            deleted: false,
        }
    );
    assert_eq!(list(&client).await, vec![created.clone()]);
    assert!(
        state.path().join("state/agents/identities.json").exists(),
        "identities live under the host's state"
    );

    ask(
        &client,
        AppAgentRequest::Rename {
            pubkey: pubkey.clone(),
            name: "  Ada Lovelace ".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(list(&client).await[0].name, "Ada Lovelace");

    let picture = "https://relay.example.com/media/ada.png";
    ask(
        &client,
        AppAgentRequest::SetPicture {
            pubkey: pubkey.clone(),
            picture: picture.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(list(&client).await[0].picture.as_deref(), Some(picture));
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::SetPicture {
                    pubkey: pubkey.clone(),
                    picture: "http://relay.example.com/media/ada.png".into(),
                },
            )
            .await
        ),
        "Use an HTTPS image URL without credentials"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::SetPicture {
                    pubkey: "d".repeat(64),
                    picture: picture.into(),
                },
            )
            .await
        ),
        "No such agent on this device"
    );

    // Another type keeps the identity, name and picture.
    ask(
        &client,
        AppAgentRequest::SetType {
            pubkey: pubkey.clone(),
            agent_type: "test/other".into(),
        },
    )
    .await
    .unwrap();
    let retyped = &list(&client).await[0];
    assert_eq!(
        (retyped.agent_type.as_str(), retyped.name.as_str()),
        ("test/other", "Ada Lovelace")
    );
    assert_eq!(retyped.picture.as_deref(), Some(picture));
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::SetType {
                    pubkey: pubkey.clone(),
                    agent_type: String::new(),
                },
            )
            .await
        ),
        "Choose an agent type"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::SetType {
                    pubkey: "d".repeat(64),
                    agent_type: "test/echo".into(),
                },
            )
            .await
        ),
        "No such agent on this device"
    );

    // Bounds checked before anything is sent.
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::Publish {
                    pubkey: pubkey.clone(),
                    event: AppAgentEvent {
                        kind: 1,
                        content: "hi".into(),
                        tags: vec![],
                    },
                },
            )
            .await
        ),
        "Agents can sign messages, edits, reactions, deletions and channel changes only"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::Publish {
                    pubkey: pubkey.clone(),
                    event: AppAgentEvent {
                        kind: 9,
                        content: "x".repeat(64 * 1024 + 1),
                        tags: vec![],
                    },
                },
            )
            .await
        ),
        "Agent event is too large"
    );
    for filters in ["[]", &format!("[{}]", ["{}"; 9].join(","))] {
        assert_eq!(
            failed(
                ask(
                    &client,
                    AppAgentRequest::Query {
                        pubkey: pubkey.clone(),
                        filters: filters.into(),
                    },
                )
                .await
            ),
            "Agents read with one to eight filters"
        );
    }
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::Upload {
                    pubkey: pubkey.clone(),
                    data: "AAAA".into(),
                    mime: "text/html".into(),
                    staged: None,
                },
            )
            .await
        ),
        "Agents upload images or MP4 video up to 50 MB"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::Upload {
                    pubkey: pubkey.clone(),
                    data: String::new(),
                    mime: "image/png".into(),
                    staged: Some("never-started".into()),
                },
            )
            .await
        ),
        "Upload expired; try again"
    );

    ask(
        &client,
        AppAgentRequest::Delete {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .unwrap();
    let deleted = list(&client).await;
    assert!(deleted[0].deleted, "the row stays until it is forgotten");
    assert_eq!(
        std::fs::read_dir(state.path().join("keys"))
            .unwrap()
            .count(),
        0,
        "the key is deleted"
    );
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::PublishProfile {
                    pubkey: pubkey.clone(),
                },
            )
            .await
        ),
        "No such agent on this device"
    );
    ask(
        &client,
        AppAgentRequest::Delete {
            pubkey: pubkey.clone(),
        },
    )
    .await
    .expect("deleting again is a success");
    ask(&client, AppAgentRequest::Forget { pubkey })
        .await
        .unwrap();
    assert!(list(&client).await.is_empty());
}

/// A host from before Agents2: the same protocol without the new variants.
mod old {
    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum Request {
        Agents,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum Response {
        Agents,
    }

    pub struct OldProtocol;

    impl Protocol for OldProtocol {
        type ClientRequest = Request;
        type ClientResponse = Response;
        type HostRequest = HostAsk;
        type HostResponse = HostAnswer;
        type Event = HostEvent;
    }

    pub struct OldHost(pub tokio::sync::broadcast::Sender<(u64, HostEvent)>);

    impl HostHandler<OldProtocol> for OldHost {
        fn info(&self) -> HostInfo {
            info()
        }

        fn events(&self) -> EventSource<OldProtocol> {
            self.0.subscribe()
        }

        fn handle(
            &self,
            _: Peer<OldProtocol>,
            Request::Agents: Request,
        ) -> BoxFuture<'static, Result<Response, RpcError>> {
            Box::pin(async { Ok(Response::Agents) })
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn old_hosts_answer_app_agent_requests_as_unsupported() {
    let (events, _) = tokio::sync::broadcast::channel(4);
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(old::OldHost(events)),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    connected(&client).await;
    let message = failed(ask(&client, AppAgentRequest::List).await);
    assert!(message.contains("does not support"), "{message}");
    let claim = AppAgentRequest::Claim {
        pubkey: "a".repeat(64),
    };
    let message = failed(ask(&client, claim).await);
    assert!(message.contains("does not support"), "{message}");
    let picture = AppAgentRequest::SetPicture {
        pubkey: "a".repeat(64),
        picture: "https://relay.example.com/media/ada.png".into(),
    };
    let message = failed(ask(&client, picture).await);
    assert!(message.contains("does not support"), "{message}");
    let retype = AppAgentRequest::SetType {
        pubkey: "a".repeat(64),
        agent_type: "test/other".into(),
    };
    let message = failed(ask(&client, retype).await);
    assert!(message.contains("does not support"), "{message}");
    // The connection survives.
    assert!(matches!(
        *client.state().borrow(),
        buzz_host_rpc::ConnectionState::Connected { .. }
    ));
}

async fn claim(client: &HostClient<HostProtocol>, pubkey: &str) -> bool {
    let request = AppAgentRequest::Claim {
        pubkey: pubkey.into(),
    };
    match ask(client, request).await.unwrap() {
        AppAgentResponse::Claimed(claimed) => claimed,
        other => panic!("expected a claim, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_gui_runs_each_agent_until_it_lets_go_or_disconnects() {
    let state = tempfile::tempdir().unwrap();
    let host = AgentHost::start(HostConfig {
        info: info(),
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let connector = Arc::new(connector);
    let gui = || {
        HostClient::<HostProtocol>::spawn(
            &tokio::runtime::Handle::current(),
            connector.clone(),
            Arc::new(NoAsks),
            HostClientOptions::default(),
        )
    };
    let (one, other) = (gui(), gui());
    connected(&one).await;
    connected(&other).await;
    let agent = "a".repeat(64);
    assert!(claim(&one, &agent).await);
    assert!(claim(&one, &agent).await, "a holder keeps it");
    assert!(!claim(&other, &agent).await);
    assert!(claim(&other, &"b".repeat(64)).await);

    let release = AppAgentRequest::Release {
        pubkey: agent.clone(),
    };
    ask(&one, release).await.unwrap();
    assert!(claim(&other, &agent).await);
    assert!(!claim(&one, &agent).await);

    // As when that GUI quits.
    drop(other);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !claim(&one, &agent).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a closed connection lets go");
    let invalid = AppAgentRequest::Claim {
        pubkey: "../x".into(),
    };
    assert_eq!(failed(ask(&one, invalid).await), "Invalid agent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v1_agent_is_brought_over_with_its_key_name_and_picture() {
    use buzz_agent_controller::{Credentials, Secret};
    use sha2::{Digest, Sha256};
    let state = tempfile::tempdir().unwrap();
    let keys = FileCredentials::new(state.path().join("keys")).unwrap();
    // A harness agent the v1 controller saved on this host.
    let key = Secret::generate().unwrap();
    let relay = "wss://old.example.com";
    let credential = format!("{}-{:x}", key.pubkey(), Sha256::digest(relay.as_bytes()));
    keys.add(&credential, &key).unwrap();
    let library = state.path().join("state/agent-controller");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(
        library.join("agents.json"),
        serde_json::json!({
            "version": 1,
            "agents": [{
                "pubkey": key.pubkey(),
                "relayUrl": relay,
                "name": "Scout",
                "picture": "https://old.example.com/media/scout.png",
                "systemPrompt": "Look around.",
            }],
        })
        .to_string(),
    )
    .unwrap();
    let host = AgentHost::start(HostConfig {
        info: info(),
        state_dir: state.path().join("state"),
        credentials: Arc::new(keys),
        legacy_dir: Some(state.path().join("data")),
        sandbox: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    connected(&client).await;

    let AppAgentResponse::Portable(found) = ask(&client, AppAgentRequest::Portable).await.unwrap()
    else {
        panic!("expected portable agents");
    };
    assert_eq!(
        found.agents,
        vec![PortableAgent {
            source: PortSource::Harness,
            pubkey: key.pubkey().into(),
            name: "Scout".into(),
            picture: Some("https://old.example.com/media/scout.png".into()),
            relay: relay.into(),
        }]
    );
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);

    let owner = Identity::ephemeral();
    let port = |request_id: &str| AppAgentRequest::PortPrepare {
        request_id: request_id.into(),
        source: PortSource::Harness,
        pubkey: key.pubkey().into(),
        destination: "wss://relay.example.com".into(),
        owner: owner.pubkey_hex(),
        agent_type: "test/echo".into(),
    };
    let AppAgentResponse::PortPrepared { pubkey, carry } =
        ask(&client, port("port-1")).await.unwrap()
    else {
        panic!("expected prepared");
    };
    assert_eq!(pubkey, key.pubkey(), "the agent keeps its key");
    assert_eq!(
        carry,
        Carry {
            instructions: Some("Look around.".into()),
            workspace: None,
        },
        "what its new type may adopt"
    );
    let AppAgentResponse::Created(created) = ask(
        &client,
        AppAgentRequest::CreateCommit {
            request_id: "port-1".into(),
            auth: owner.attest_agent(&pubkey).unwrap(),
        },
    )
    .await
    .unwrap() else {
        panic!("expected created");
    };
    assert_eq!(
        created,
        AppAgentSummary {
            pubkey: pubkey.clone(),
            relay: "wss://relay.example.com".into(),
            owner: owner.pubkey_hex(),
            agent_type: "test/echo".into(),
            name: "Scout".into(),
            picture: Some("https://old.example.com/media/scout.png".into()),
            deleted: false,
        }
    );
    // Its old store keeps it, and bringing it over again is refused.
    let AppAgentResponse::Portable(again) = ask(&client, AppAgentRequest::Portable).await.unwrap()
    else {
        panic!("expected portable agents");
    };
    assert_eq!(again.agents.len(), 1);
    let AppAgentResponse::PortPrepared { .. } = ask(&client, port("port-2")).await.unwrap() else {
        panic!("expected prepared");
    };
    assert_eq!(
        failed(
            ask(
                &client,
                AppAgentRequest::CreateCommit {
                    request_id: "port-2".into(),
                    auth: owner.attest_agent(&pubkey).unwrap(),
                },
            )
            .await
        ),
        "That agent is already here"
    );
}

#[test]
fn the_agents2_folder_moves_to_agents_once() {
    let state = tempfile::tempdir().unwrap();
    let old = state.path().join("agents2");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("identities.json"), "[]").unwrap();
    buzz_host::migrate_agents(state.path()).unwrap();
    assert!(!old.exists());
    assert!(state.path().join("agents/identities.json").exists());
    // Nothing to move the second time.
    buzz_host::migrate_agents(state.path()).unwrap();
    // Both folders are a conflict, and nothing moves.
    std::fs::create_dir_all(&old).unwrap();
    assert_eq!(
        buzz_host::migrate_agents(state.path()).unwrap_err(),
        "Both agents and agents2 storage exist. Resolve the conflicting folders before \
         reopening Buzz; nothing was moved."
    );
    assert!(old.exists());
}
