//! buzz-app's `rpc.test.ts`.

use super::*;
use crate::agent_plugins::process::{ProcessControl, Spawn, Spawned};

/// A server that answers only `initialize`; the test writes the rest.
#[derive(Clone, Default)]
struct Fixture(Arc<Mutex<FixtureState>>);

#[derive(Default)]
struct FixtureState {
    sent: Vec<Value>,
    output: Option<mpsc::UnboundedSender<ProcessOutput>>,
    ended: usize,
}

impl Fixture {
    fn raw(&self, text: &str) {
        let output = self.0.lock().unwrap().output.clone().unwrap();
        let _ = output.send(ProcessOutput::Stdout(text.into()));
    }
    fn emit(&self, message: Value) {
        self.raw(&format!("{message}\n"));
    }
    fn sent(&self) -> Vec<Value> {
        self.0.lock().unwrap().sent.clone()
    }
}

impl ProcessControl for Fixture {
    fn write(&self, data: String, close: bool) -> BoxFuture<'static, Result<(), String>> {
        if close {
            let mut state = self.0.lock().unwrap();
            state.ended += 1;
            if let Some(output) = state.output.take() {
                let _ = output.send(ProcessOutput::Exit(Some(0)));
            }
        }
        for line in data.lines() {
            let message: Value = serde_json::from_str(line).unwrap();
            self.0.lock().unwrap().sent.push(message.clone());
            if message["method"] == "initialize" {
                self.emit(json!({ "id": message["id"], "result": {} }));
            }
        }
        Box::pin(async { Ok(()) })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        if let Some(output) = self.0.lock().unwrap().output.take() {
            let _ = output.send(ProcessOutput::Exit(None));
        }
        Box::pin(async { Ok(()) })
    }
}

impl Spawn for Fixture {
    fn spawn(&self, _: &str, _: SpawnOptions) -> BoxFuture<'static, Result<Spawned, String>> {
        let (output, receiver) = mpsc::unbounded_channel();
        self.0.lock().unwrap().output = Some(output);
        let process = Process(Arc::new(self.clone()));
        Box::pin(async move {
            Ok(Spawned {
                process,
                output: receiver,
            })
        })
    }
}

async fn opened() -> (Fixture, AppServer) {
    let fixture = Fixture::default();
    let rpc = AppServer::new(None);
    let spawn: Spawner = Arc::new(fixture.clone());
    rpc.open(&spawn, None).await.unwrap();
    (fixture, rpc)
}

fn counter() -> (Arc<Mutex<Vec<Value>>>, Listener) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let listener: Listener = {
        let heard = heard.clone();
        Arc::new(move |message: &Value| heard.lock().unwrap().push(message.clone()))
    };
    (heard, listener)
}

#[tokio::test]
async fn correlates_out_of_order_lines_across_chunks_and_refuses_interactive_requests() {
    let (fixture, rpc) = opened().await;
    let initialize = &fixture.sent()[0];
    assert_eq!(
        initialize["params"]["capabilities"]["experimentalApi"],
        true
    );
    assert_eq!(fixture.sent()[1], json!({ "method": "initialized" }));
    let (first, second) = (rpc.clone(), rpc.clone());
    let first = tokio::spawn(async move { first.request("first", json!({})).await });
    let second = tokio::spawn(async move { second.request("second", json!({})).await });
    crate::agent_plugins::codex::rpc::testing::until(|| fixture.sent().len() == 4).await;
    let id = |method: &str| {
        fixture
            .sent()
            .iter()
            .find(|message| message["method"] == method)
            .map(|message| message["id"].clone())
            .unwrap()
    };
    let text = format!(
        "{}\n{}\n",
        json!({ "id": id("second"), "result": "second result" }),
        json!({ "id": id("first"), "result": "first result" }),
    );
    fixture.raw(&text[..11]);
    fixture.raw(&text[11..]);
    assert_eq!(first.await.unwrap(), Ok(json!("first result")));
    assert_eq!(second.await.unwrap(), Ok(json!("second result")));

    let (one, listener) = counter();
    rpc.on_message(listener);
    fixture
        .emit(json!({ "id": "interactive", "method": "item/tool/requestUserInput", "params": {} }));
    crate::agent_plugins::codex::rpc::testing::until(|| {
        fixture
            .sent()
            .iter()
            .any(|message| message["id"] == "interactive")
    })
    .await;
    let refusals: Vec<Value> = fixture
        .sent()
        .into_iter()
        .filter(|message| message["id"] == "interactive")
        .collect();
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0]["error"]["code"], -32601);
    assert!(one.lock().unwrap().is_empty());
    rpc.close().await;
}

#[tokio::test]
async fn answers_tool_calls_with_their_handler() {
    let fixture = Fixture::default();
    let tool: ToolHandler = Arc::new(|call: ToolCall| {
        async move {
            match call.tool.as_str() {
                "send" => Ok(Reply::Text(format!("sent {}", call.arguments["text"]))),
                _ => Err("No such tool".to_string()),
            }
        }
        .boxed()
    });
    let rpc = AppServer::new(Some(tool));
    let spawn: Spawner = Arc::new(fixture.clone());
    rpc.open(&spawn, None).await.unwrap();
    for (id, tool) in [("a", "send"), ("b", "other")] {
        fixture.emit(json!({
            "id": id,
            "method": "item/tool/call",
            "params": {
                "threadId": "t", "turnId": "u", "callId": id, "namespace": "buzz",
                "tool": tool, "arguments": { "text": "hi" },
            },
        }));
    }
    let reply = |id: &str| {
        fixture
            .sent()
            .into_iter()
            .find(|message| message["id"] == id && message.get("method").is_none())
    };
    crate::agent_plugins::codex::rpc::testing::until(|| {
        reply("a").is_some() && reply("b").is_some()
    })
    .await;
    assert_eq!(
        reply("a").unwrap()["result"],
        json!({ "success": true, "contentItems": [{ "type": "inputText", "text": "sent \"hi\"" }] })
    );
    assert_eq!(
        reply("b").unwrap()["result"],
        json!({ "success": false, "contentItems": [{ "type": "inputText", "text": "No such tool" }] })
    );
    rpc.close().await;
}

#[tokio::test]
async fn contains_listener_failures_and_closes_on_malformed_output() {
    let (fixture, rpc) = opened().await;
    rpc.on_message(Arc::new(|_: &Value| panic!("consumer failure")));
    let (heard, listener) = counter();
    rpc.on_message(listener);
    fixture.emit(json!({ "method": "turn/started", "params": {} }));
    crate::agent_plugins::codex::rpc::testing::until(|| !heard.lock().unwrap().is_empty()).await;
    assert_eq!(
        heard.lock().unwrap()[0],
        json!({ "method": "turn/started", "params": {} })
    );
    let waiting = {
        let rpc = rpc.clone();
        tokio::spawn(async move { rpc.request("waiting", json!({})).await })
    };
    crate::agent_plugins::codex::rpc::testing::until(|| fixture.sent().len() == 3).await;
    fixture.raw("invalid json\n");
    assert!(waiting.await.unwrap().is_err());
    // The listener heard the connection close, and later requests fail.
    crate::agent_plugins::codex::rpc::testing::until(|| {
        heard
            .lock()
            .unwrap()
            .iter()
            .any(|message| message["method"] == "connection/closed")
    })
    .await;
    assert!(rpc.request("later", json!({})).await.is_err());
    rpc.close().await;
    rpc.close().await;
    assert_eq!(fixture.0.lock().unwrap().ended, 1);
}

#[tokio::test]
async fn an_exit_fails_waiting_requests_with_what_codex_said() {
    let (fixture, rpc) = opened().await;
    let waiting = {
        let rpc = rpc.clone();
        tokio::spawn(async move { rpc.request("waiting", json!({})).await })
    };
    crate::agent_plugins::codex::rpc::testing::until(|| fixture.sent().len() == 3).await;
    let output = fixture.0.lock().unwrap().output.take().unwrap();
    let _ = output.send(ProcessOutput::Stderr("Not signed in\n".into()));
    let _ = output.send(ProcessOutput::Exit(Some(1)));
    assert_eq!(waiting.await.unwrap(), Err("Not signed in".into()));
    // Closing a server that already exited does not wait.
    rpc.close().await;
}

#[tokio::test]
async fn lists_models_page_by_page() {
    let (fixture, rpc) = opened().await;
    let listing = {
        let rpc = rpc.clone();
        tokio::spawn(async move { list_models(&rpc).await })
    };
    for (page, cursor) in [(1, json!("next")), (2, Value::Null)] {
        crate::agent_plugins::codex::rpc::testing::until(|| fixture.sent().len() == 2 + page).await;
        let request = fixture.sent().last().cloned().unwrap();
        assert_eq!(request["method"], "model/list");
        assert_eq!(request["params"]["limit"], 100);
        fixture.emit(json!({
            "id": request["id"],
            "result": { "data": [{ "model": format!("m{page}"), "isDefault": page == 1 }], "nextCursor": cursor },
        }));
    }
    let models = listing.await.unwrap().unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2"]
    );
    assert!(models[0].is_default);
    rpc.close().await;
}
