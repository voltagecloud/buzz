use super::*;
use buzz_sdk::transcript::{
    build_agent_session_transcript, ContextWindow, TranscriptEnvelope, TranscriptPayload,
    TranscriptUsage, WorkspaceEntry,
};

fn root(ids: &Ids) -> Event {
    buzz_sdk::build_message(
        ids.channel,
        "change this file",
        None,
        &[&ids.agent.public_key().to_hex()],
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(100))
    .sign_with_keys(&ids.signer)
    .unwrap()
}

fn payload(root: EventId) -> TranscriptPayload {
    TranscriptPayload {
        v: 1,
        turn_id: "turn-replay".into(),
        turn_seq: 1,
        thread_root: root.to_hex(),
        started_at: "1970-01-01T00:01:41Z".into(),
        ended_at: "1970-01-01T00:01:45Z".into(),
        stop_reason: "end_turn".into(),
        usage: Some(TranscriptUsage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            total_tokens: Some(14),
            ..Default::default()
        }),
        context: Some(ContextWindow {
            size: 200000,
            used: 20,
        }),
        workspace: vec![WorkspaceEntry {
            path: "/home/agent/backend".into(),
            repo: "voltagecloud/backend".into(),
            branch: Some("feat/replay".into()),
            head: "a".repeat(40),
            pushed: true,
        }],
        omitted: 0,
        updates: vec![
            json!({"sessionUpdate":"plan","entries":[{"content":"edit file","priority":"medium","status":"completed"}]}),
            json!({"sessionUpdate":"tool_call","toolCallId":"edit-1","title":"Edit main.rs","kind":"edit","status":"completed",
                "locations":[{"path":"/home/agent/backend/src/main.rs","line":1}],
                "rawInput":{"file_path":"/home/agent/backend/src/main.rs"},
                "content":[{"type":"diff","path":"/home/agent/backend/src/main.rs","oldText":"old","newText":"new"}]}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}),
        ],
    }
}

fn part(
    ids: &Ids,
    author: &Keys,
    recipient: PublicKey,
    payload: &TranscriptPayload,
    index: usize,
    count: usize,
) -> Event {
    let encrypted = nostr::nips::nip44::encrypt(
        author.secret_key(),
        &recipient,
        serde_json::to_string(payload).unwrap(),
        nostr::nips::nip44::Version::V2,
    )
    .unwrap();
    build_agent_session_transcript(
        &TranscriptEnvelope {
            recipient,
            thread_root: EventId::from_hex(&payload.thread_root).unwrap(),
            channel: ids.channel,
            turn_id: payload.turn_id.clone(),
            part_index: index,
            part_count: count,
        },
        &encrypted,
    )
    .unwrap()
    .sign_with_keys(author)
    .unwrap()
}

fn answer(ids: &Ids, root: EventId, content: &str) -> Event {
    buzz_sdk::build_message(
        ids.channel,
        content,
        Some(&ThreadRef {
            root_event_id: root,
            parent_event_id: root,
        }),
        &[],
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(104))
    .sign_with_keys(&ids.agent)
    .unwrap()
}

/// The real stdio bridge rebuilds complete encrypted turns after restart,
/// preserves distinct posts, rewrites file links, and falls back on bad parts.
#[tokio::test]
async fn history_replays_transcripts_and_keeps_text_when_parts_are_unusable() {
    for mode in [
        "complete",
        "missing",
        "conflicting",
        "wrong_author",
        "wrong_recipient",
    ] {
        let ids = Ids::new();
        let root = root(&ids);
        let mut transcript = payload(root.id);
        if mode == "shortened" {
            transcript.updates[1]["content"][0]["newText"] = json!("…[elided 9000 bytes]…");
        }
        let mut second = transcript.clone();
        second.updates = transcript.updates.split_off(2);
        let stranger = Keys::generate();
        let author = if mode == "wrong_author" {
            &stranger
        } else {
            &ids.agent
        };
        let recipient = if mode == "wrong_recipient" {
            stranger.public_key()
        } else {
            ids.signer.public_key()
        };
        let mut stored = vec![
            root.clone(),
            answer(&ids, root.id, "done"),
            answer(&ids, root.id, "a distinct durable post"),
            part(&ids, author, recipient, &transcript, 0, 2),
        ];
        if mode != "missing" {
            stored.push(part(&ids, author, recipient, &second, 1, 2));
        }
        if mode == "conflicting" {
            transcript.updates[0]["entries"][0]["content"] = json!("contradicting copy");
            stored.push(part(&ids, author, recipient, &transcript, 0, 2));
        }
        let mut harness = start(&ids, Timing::default());
        harness.relay_tx.send(RelayInbound::Live).await.unwrap();
        harness.history.lock().unwrap().0 = stored;
        let (response, notes) = harness
            .client
            .request(1, "session/load", json!({"sessionId":root.id.to_hex()}))
            .await;
        assert!(response.get("result").is_some(), "{mode}: {response}");
        let updates: Vec<_> = notes
            .iter()
            .filter_map(|note| note.pointer("/params/update"))
            .collect();
        let tool = updates
            .iter()
            .find(|update| update["sessionUpdate"] == "tool_call");
        let complete = mode == "complete";
        assert_eq!(tool.is_some(), complete, "{mode}");
        assert!(
            texts(&notes)
                .iter()
                .any(|text| text.contains("a distinct durable post")),
            "{mode}"
        );
        assert_eq!(
            texts(&notes)
                .iter()
                .filter(|text| text.trim() == "done")
                .count(),
            1,
            "{mode}"
        );
        assert_eq!(
            updates[0]["sessionUpdate"], "user_message_chunk",
            "prompt precedes turn"
        );
        if tool.is_some() {
            let usage = updates
                .iter()
                .find(|update| update["sessionUpdate"] == "usage_update")
                .unwrap();
            assert_eq!(usage["used"], 20);
            assert_eq!(usage["_meta"]["usage"]["totalTokens"], 14);
        }
        harness.bridge.abort();
    }
}
