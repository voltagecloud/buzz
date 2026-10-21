//! The Buzz tools as an MCP server inside the app (buzz-app's
//! `src/buzz-mcp/rpc.ts`): answers one JSON-RPC message at a time, as
//! Claude sends them over its stream-json pipe to an in-process
//! (`sdk`) server. Holds no state between messages.

use super::attention::{Attention, attention_tools, call_attention_tool, is_attention_tool};
use super::tools::{Reply, call_tool, tools};
use super::{BuzzClient, Context};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// The attention tool set for one session. `offered` decides the tool
/// list, which a session reads once, so it is fixed when the session
/// starts; `api` is the agent's attention once a delivery has given the
/// session a handle.
#[derive(Clone, Default)]
pub struct AttentionTools {
    pub offered: bool,
    pub api: Option<Arc<dyn Attention>>,
}

/// Answers `input`. Without a client, tools are listed but cannot be called.
pub async fn respond(
    client: Option<Arc<dyn BuzzClient>>,
    context: &Context,
    input: &Value,
) -> Value {
    respond_with(client, context, input, None).await
}

/// [`respond`], listing the attention tools only when `attention` offers
/// them.
pub async fn respond_with(
    client: Option<Arc<dyn BuzzClient>>,
    context: &Context,
    input: &Value,
    attention: Option<&AttentionTools>,
) -> Value {
    let offered = attention.is_some_and(|attention| attention.offered);
    let id = input.get("id").cloned().unwrap_or(Value::Null);
    // A notification needs no answer, but the pipe expects one.
    if id.is_null() {
        return json!({ "jsonrpc": "2.0", "id": 0, "result": {} });
    }
    let empty = Map::new();
    let params = input
        .get("params")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let result = |value: Value| json!({ "jsonrpc": "2.0", "id": id, "result": value });
    match input.get("method").and_then(Value::as_str) {
        Some("initialize") => result(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "buzz", "version": "1" },
        })),
        Some("ping") => result(json!({})),
        Some("tools/list") => {
            let mut listed = tools();
            if offered {
                listed.extend(attention_tools());
            }
            result(json!({ "tools": listed }))
        }
        Some("tools/call") => {
            let called = async {
                let name = match params.get("name") {
                    Some(Value::String(name)) => name.clone(),
                    Some(other) => other.to_string(),
                    None => "undefined".into(),
                };
                let args = params
                    .get("arguments")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if offered && is_attention_tool(&name) {
                    let api = attention
                        .and_then(|attention| attention.api.clone())
                        .ok_or("Buzz is not ready yet. Try again.")?;
                    // The owner can turn attention off while this session
                    // runs.
                    if !api.enabled().await? {
                        let classifier = match api.classifier() {
                            crate::agents2::objects::ClassifierAvailability::Available => {
                                "available"
                            }
                            crate::agents2::objects::ClassifierAvailability::Unavailable => {
                                "unavailable"
                            }
                        };
                        return Err(format!(
                            "Attention is off for you: your owner turned it off, so these tools do nothing now.\n(classifier: {classifier})"
                        ));
                    }
                    return call_attention_tool(api.as_ref(), &name, &args)
                        .await
                        .map(Reply::Text);
                }
                let client = client.ok_or("Buzz is not ready yet. Try again.")?;
                call_tool(client, context, &name, &args).await
            };
            match called.await {
                Ok(reply) => result(json!({ "content": content(reply) })),
                Err(text) => result(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": true,
                })),
            }
        }
        method => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("No method {}", method.unwrap_or("undefined")),
            },
        }),
    }
}

/// A tool's answer as MCP content.
pub fn content(reply: Reply) -> Value {
    match reply {
        Reply::Text(text) => json!([{ "type": "text", "text": text }]),
        Reply::Image { data, mime } => json!([{ "type": "image", "data": data, "mimeType": mime }]),
    }
}
