//! Newline-delimited JSON-RPC 2.0 framing for the ACP stdio transport.
//!
//! This follows `crates/buzz-agent/src/wire.rs`: bounded line reads, one
//! writer task that owns stdout, and small response builders. That module is
//! private to the buzz-agent crate, which also brings its LLM and MCP stack,
//! so the bridge keeps this small copy instead of depending on it.

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

pub(crate) const PARSE_ERROR: i32 = -32700;
pub(crate) const INVALID_REQUEST: i32 = -32600;
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
pub(crate) const INVALID_PARAMS: i32 = -32602;
pub(crate) const INTERNAL_ERROR: i32 = -32603;

/// Largest inbound JSON-RPC line the bridge accepts (3 MiB, including two base64 image blocks). Longer lines are
/// discarded up to their newline and answered with a parse error.
pub(crate) const MAX_LINE_BYTES: usize = 3 * 1024 * 1024;

/// One frame read from the ACP client.
#[derive(Debug)]
pub(crate) enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A response to a request. The bridge never sends client requests, so
    /// these are ignored.
    Response,
    Invalid {
        id: Value,
        code: i32,
        message: String,
    },
}

/// Messages from the stdin reader task to the bridge loop.
#[derive(Debug)]
pub(crate) enum StdinMsg {
    Frame(Inbound),
    Eof,
    Failed(String),
}

enum Line {
    Text(String),
    TooLong,
    Eof,
}

/// Read one line of at most `max` bytes. An oversized line is consumed up to
/// and including its newline and reported as [`Line::TooLong`], so the stream
/// stays in sync and the next frame parses normally.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    input: &mut R,
    max: usize,
) -> std::io::Result<Line> {
    let mut buf: Vec<u8> = Vec::new();
    let mut discarding = false;
    loop {
        let chunk = input.fill_buf().await?;
        if chunk.is_empty() {
            if discarding || !buf.is_empty() {
                eprintln!("acp-bridge: dropped an unterminated frame at end of input");
            }
            return Ok(Line::Eof);
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let take = newline.map_or(chunk.len(), |i| i + 1);
        if !discarding && buf.len().saturating_add(take) > max {
            discarding = true;
            buf = Vec::new();
        }
        if !discarding {
            buf.extend_from_slice(&chunk[..take]);
        }
        input.consume(take);
        if newline.is_none() {
            continue;
        }
        if discarding {
            return Ok(Line::TooLong);
        }
        buf.pop();
        if buf.ends_with(b"\r") {
            buf.pop();
        }
        return String::from_utf8(buf).map(Line::Text).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame contains invalid UTF-8",
            )
        });
    }
}

/// Classify one JSON-RPC text frame.
pub(crate) fn classify(text: &str) -> Inbound {
    let msg: Value = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => {
            return Inbound::Invalid {
                id: Value::Null,
                code: PARSE_ERROR,
                message: format!("parse error: {e}"),
            }
        }
    };
    if !msg.is_object() || msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Inbound::Invalid {
            id: msg.get("id").cloned().unwrap_or(Value::Null),
            code: INVALID_REQUEST,
            message: "jsonrpc: missing or invalid version".into(),
        };
    }
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Inbound::Request { id, method, params },
        (Some(method), None) => Inbound::Notification { method, params },
        (None, Some(_)) => Inbound::Response,
        (None, None) => Inbound::Invalid {
            id: Value::Null,
            code: INVALID_REQUEST,
            message: "jsonrpc: missing method and id".into(),
        },
    }
}

/// Read frames from `input` until end of input or a read failure.
pub(crate) async fn read_frames<R: AsyncBufRead + Unpin>(mut input: R, tx: mpsc::Sender<StdinMsg>) {
    loop {
        let msg = match read_bounded_line(&mut input, MAX_LINE_BYTES).await {
            Ok(Line::Text(text)) if text.trim().is_empty() => continue,
            Ok(Line::Text(text)) => StdinMsg::Frame(classify(&text)),
            Ok(Line::TooLong) => StdinMsg::Frame(Inbound::Invalid {
                id: Value::Null,
                code: PARSE_ERROR,
                message: format!("frame exceeds {MAX_LINE_BYTES} bytes"),
            }),
            Ok(Line::Eof) => StdinMsg::Eof,
            Err(e) => StdinMsg::Failed(e.to_string()),
        };
        let last = matches!(msg, StdinMsg::Eof | StdinMsg::Failed(_));
        if tx.send(msg).await.is_err() || last {
            return;
        }
    }
}

/// Write each queued frame to `out` as one JSON line. Returns on the first
/// write or flush failure; dropping `rx` then makes every later send fail, so
/// the bridge loop stops instead of writing into a dead pipe.
pub(crate) async fn write_frames<W: AsyncWrite + Unpin>(mut rx: mpsc::Receiver<Value>, mut out: W) {
    while let Some(frame) = rx.recv().await {
        let mut line = match serde_json::to_string(&frame) {
            Ok(line) => line,
            Err(e) => {
                eprintln!("acp-bridge: could not serialize an outbound frame: {e}");
                continue;
            }
        };
        line.push('\n');
        if let Err(e) = out.write_all(line.as_bytes()).await {
            eprintln!("acp-bridge: stdout write failed: {e}");
            return;
        }
        if let Err(e) = out.flush().await {
            eprintln!("acp-bridge: stdout flush failed: {e}");
            return;
        }
    }
}

pub(crate) fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub(crate) fn err(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub(crate) fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update },
    })
}

/// An `agent_message_chunk` update carrying `text`.
pub(crate) fn agent_text_chunk(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// An `available_commands_update` advertising `(name, description)` slash
/// commands. An empty list withdraws earlier commands.
pub(crate) fn available_commands(commands: &[(&str, &str)]) -> Value {
    let commands: Vec<Value> = commands
        .iter()
        .map(|(name, description)| json!({ "name": name, "description": description }))
        .collect();
    json!({
        "sessionUpdate": "available_commands_update",
        "availableCommands": commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_line_is_skipped_and_the_next_frame_still_parses() {
        let mut input = "x".repeat(32);
        input.push('\n');
        input.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"m\"}\n");
        let mut reader = tokio::io::BufReader::with_capacity(8, input.as_bytes());
        assert!(matches!(
            read_bounded_line(&mut reader, 16).await.unwrap(),
            Line::TooLong
        ));
        let Line::Text(text) = read_bounded_line(&mut reader, 16 * 4).await.unwrap() else {
            panic!("expected the following frame");
        };
        assert!(matches!(classify(&text), Inbound::Notification { .. }));
        assert!(matches!(
            read_bounded_line(&mut reader, 16).await.unwrap(),
            Line::Eof
        ));
    }

    #[test]
    fn classify_rejects_non_jsonrpc_frames() {
        assert!(matches!(
            classify("not json"),
            Inbound::Invalid {
                code: PARSE_ERROR,
                ..
            }
        ));
        assert!(matches!(
            classify("{\"id\":1,\"method\":\"initialize\"}"),
            Inbound::Invalid {
                code: INVALID_REQUEST,
                ..
            }
        ));
    }
}
