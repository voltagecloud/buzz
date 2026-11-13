use super::*;
use nostr::Keys;

struct NoRelay;
impl BridgeRelay for NoRelay {
    async fn prepare_prompt(
        &self,
        _content: String,
        _root: Option<String>,
    ) -> Result<Event, CliError> {
        Err(problem("unexpected relay call"))
    }
    async fn submit(&self, _event: Event) -> Result<(), CliError> {
        Err(problem("unexpected relay call"))
    }
    async fn publish_ephemeral(&self, _event: Event) -> Result<(), CliError> {
        Err(problem("unexpected relay call"))
    }
}

fn sample_bridge() -> (Bridge<NoRelay>, mpsc::Receiver<Value>) {
    let signer = Keys::generate();
    let scope = Scope {
        owner: signer.public_key(),
        signer,
        agent: Keys::generate().public_key(),
        channel: uuid::Uuid::new_v4(),
        home: std::path::PathBuf::from("/nonexistent"),
        paseo_code: std::path::PathBuf::from("/nonexistent/paseo-code"),
    };
    let (out, replies) = mpsc::channel(4);
    let (ops, _) = mpsc::channel(4);
    let bridge = Bridge {
        views: BranchViews::new(scope.paseo_code.clone(), scope.home.clone()),
        scope,
        relay: Arc::new(NoRelay),
        out,
        ops,
        timing: Timing::default(),
        sessions: HashMap::new(),
        history_ops: 1,
        loading: HashMap::from([(
            "missing".to_owned(),
            HistoryLoad {
                token: 1,
                overflowed: false,
                project: None,
            },
        )]),
        history_live: Vec::new(),
        history_live_bytes: 0,
        relay_live: false,
        relay_error: None,
        seen: RecentIds::new(SEEN_EVENT_CAP),
        tick: 0,
        names: Arc::new(Names::default()),
        peer_names: 0,
        prepares: Arc::new(Semaphore::new(4)),
    };
    (bridge, replies)
}

/// An invalid completion is an ACP error response, not a fatal loop error.
#[tokio::test]
async fn missing_root_completion_answers_error_and_keeps_session_creation_available() {
    let (mut bridge, mut replies) = sample_bridge();
    assert!(bridge
        .finish_history_load(
            json!(7),
            "missing".into(),
            1,
            None,
            Ok(Replay {
                turns: Vec::new(),
                events: Vec::new(),
                labels: HashMap::new()
            })
        )
        .await
        .is_ok());
    let error = replies.recv().await.unwrap();
    assert_eq!(error["id"], 7);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing a valid thread root"));
    assert!(bridge.sessions.is_empty());
    bridge.on_session_new(json!(8), &json!({})).await.unwrap();
    let next = replies.recv().await.unwrap();
    assert_eq!(next["id"], 8);
    assert!(next["result"]["sessionId"].is_string());
}

#[tokio::test]
async fn delayed_peer_name_cannot_duplicate_a_newer_replay() {
    let (mut bridge, mut replies) = sample_bridge();
    bridge.on_session_new(json!(1), &json!({})).await.unwrap();
    let response = replies.recv().await.unwrap();
    let session = response["result"]["sessionId"].as_str().unwrap().to_owned();
    let state = bridge.sessions.get_mut(&session).unwrap();
    state.replay_epoch = 2;
    bridge.peer_names = 1;
    bridge.on_op(Op::PeerNameWarmed).await.unwrap();
    assert!(replies.try_recv().is_err());
    assert_eq!(bridge.peer_names, 0);
    assert_eq!(bridge.sessions[&session].replay_epoch, 2);
    bridge.sessions.remove(&session);
    bridge.on_op(Op::PeerNameWarmed).await.unwrap();
    assert!(replies.try_recv().is_err());
    assert_eq!(bridge.peer_names, 0);
}
