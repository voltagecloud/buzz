use super::*;
use buzz_sdk::transcript::is_kept_update;
use nostr::ToBech32;

fn update_in(session: &str, update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update",
           "params": {"sessionId": session, "update": update}})
}

fn update(update: Value) -> Value {
    update_in("s", update)
}

fn chunk(kind: &str, text: &str) -> Value {
    update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}))
}

fn prompt(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
           "params": {"sessionId": "s", "prompt": []}})
}

fn header() -> TranscriptPayload {
    TranscriptPayload {
        v: PAYLOAD_VERSION,
        turn_id: "turn-1".into(),
        turn_seq: 1,
        thread_root: "ab".repeat(32),
        started_at: "2026-11-13T10:00:00Z".into(),
        ended_at: "2026-11-13T10:01:00Z".into(),
        stop_reason: "end_turn".into(),
        usage: None,
        context: None,
        workspace: Vec::new(),
        omitted: 0,
        updates: Vec::new(),
    }
}

/// Only the kept kinds of the prompted session survive, consecutive chunks of
/// one kind coalesce, the outcome comes from the newest prompt's response,
/// and nothing is recorded after the recorder closes.
/// Mutations: drop the kind filter → `user_message_chunk` survives; drop the
/// prompt-id fence → the setup prompt's `max_tokens` wins; drop coalescing →
/// the "Hel"+"lo" run stays split; drop the session fence → the replayed
/// history and the other session's chunk survive.
#[test]
fn recorder_keeps_coalesces_and_fences_the_turn_outcome() {
    let mut recorder = TurnRecorder::default();
    assert!(
        TurnRecorder::default().finish().is_none(),
        "no prompt, no transcript"
    );

    // A `session/load` history replay before any prompt is not this turn.
    recorder.record_read(&chunk("agent_message_chunk", "replayed history"));
    // A setup prompt earlier in the same turn.
    recorder.record_write(&prompt(1));
    recorder.record_read(&chunk("agent_message_chunk", "setup done"));
    recorder.record_read(&json!({"jsonrpc": "2.0", "id": 1,
        "result": {"stopReason": "max_tokens", "usage": {"inputTokens": 99}}}));

    recorder.record_write(&prompt(2));
    for message in [
        chunk("user_message_chunk", "the user's words"),
        chunk("agent_thought_chunk", "think"),
        // Another session on the same connection is not this turn either.
        update_in(
            "other",
            json!({"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "another session"}}),
        ),
        chunk("agent_message_chunk", "Hel"),
        chunk("agent_message_chunk", "lo"),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit",
                      "kind": "edit", "status": "pending",
                      "locations": [{"path": "/home/agent/repo/a.rs"}]}),
        ),
        update(json!({"sessionUpdate": "usage_update", "size": 200000, "used": 1234})),
        update(json!({"sessionUpdate": "available_commands_update", "availableCommands": []})),
        update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                      "status": "completed"}),
        ),
        chunk("agent_message_chunk", "after the tool"),
        // A response to some other request does not end the prompt.
        json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "refusal"}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"stopReason": "end_turn",
               "usage": {"inputTokens": 12, "outputTokens": 7, "totalTokens": 19,
                         "thoughtTokens": 3, "cachedReadTokens": 0, "costUsd": 0.5}}}),
    ] {
        recorder.record_read(&message);
    }
    let turn = recorder.finish().expect("prompted turn");
    recorder.record_read(&chunk("agent_message_chunk", "late"));
    assert!(
        recorder.finish().is_none(),
        "a closed recorder finishes once"
    );

    let summary: Vec<(String, String)> = turn
        .updates
        .iter()
        .map(|u| {
            (
                u["sessionUpdate"].as_str().unwrap().to_owned(),
                u.pointer("/content/text")
                    .or(u.get("toolCallId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect();
    let expected = [
        ("agent_message_chunk", "setup done"),
        ("agent_thought_chunk", "think"),
        ("agent_message_chunk", "Hello"),
        ("tool_call", "t1"),
        ("tool_call_update", "t1"),
        ("agent_message_chunk", "after the tool"),
    ];
    assert_eq!(
        summary,
        expected
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<Vec<_>>()
    );
    assert!(turn.updates.iter().all(is_kept_update));
    assert_eq!(turn.stop_reason, "end_turn");
    assert_eq!(
        turn.usage,
        Some(TranscriptUsage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            total_tokens: Some(19),
            cached_read_tokens: Some(0),
            cached_write_tokens: None,
        })
    );
    assert_eq!(
        turn.context,
        Some(ContextWindow {
            size: 200_000,
            used: 1234
        })
    );
    assert_eq!(turn.omitted, 0);

    // A prompt that never gets an answer ends as an error.
    let mut unanswered = TurnRecorder::default();
    unanswered.record_write(&prompt(3));
    assert_eq!(unanswered.finish().unwrap().stop_reason, "error");
}

/// Coalesced chunk text is bounded while streaming, yet ends identical to
/// eliding the whole text at once, for short, boundary, long, and multi-byte
/// text. Non-chunk string leaves over 8 KiB are elided the same way.
#[test]
fn coalesced_text_matches_whole_text_elision_with_bounded_memory() {
    for (piece, count) in [
        ("a", 10),
        ("abcdefgh", 1024),
        ("abcdefgh", 1025),
        ("x", 100_000),
        ("héllo wörld ✓ ", 9_000),
    ] {
        let mut recorder = TurnRecorder::default();
        recorder.record_write(&prompt(1));
        for _ in 0..count {
            recorder.record_read(&chunk("agent_message_chunk", piece));
            assert!(
                recorder.bytes <= CHUNK_COMPACT_BYTES + 512,
                "chunk text is compacted while streaming"
            );
        }
        let turn = recorder.finish().unwrap();
        let mut whole = Value::String(piece.repeat(count));
        elide_large_leaves(&mut whole);
        assert_eq!(turn.updates.len(), 1);
        assert_eq!(
            turn.updates[0]["content"]["text"], whole,
            "{piece} x {count}"
        );
        let len = whole.as_str().unwrap().len();
        assert!(len <= LEAF_LIMIT_BYTES, "{piece} x {count}: {len} bytes");
    }

    let mut recorder = TurnRecorder::default();
    recorder.record_write(&prompt(1));
    recorder.record_read(&update(
        json!({"sessionUpdate": "tool_call", "toolCallId": "t",
        "rawInput": {"command": "y".repeat(LEAF_LIMIT_BYTES)},
        "rawOutput": "z".repeat(LEAF_LIMIT_BYTES + 1)}),
    ));
    let turn = recorder.finish().unwrap();
    assert_eq!(
        turn.updates[0]["rawInput"]["command"]
            .as_str()
            .unwrap()
            .len(),
        LEAF_LIMIT_BYTES,
        "a leaf of exactly 8 KiB is kept"
    );
    assert!(turn.updates[0]["rawOutput"]
        .as_str()
        .unwrap()
        .contains("…[elided "));
}

/// The recorder stops at its hard bounds and counts what it drops.
#[test]
fn recorder_is_bounded_and_counts_dropped_updates() {
    let mut recorder = TurnRecorder::default();
    recorder.record_write(&prompt(1));
    for index in 0..MAX_RECORDED_UPDATES + 10 {
        recorder.record_read(&update(json!({"sessionUpdate": "plan",
            "entries": [{"content": format!("step {index}")}]})));
    }
    let turn = recorder.finish().unwrap();
    assert_eq!(turn.updates.len(), MAX_RECORDED_UPDATES);
    assert_eq!(turn.omitted, 10);

    let mut recorder = TurnRecorder::default();
    recorder.record_write(&prompt(1));
    for index in 0..1000 {
        recorder.record_read(&update(json!({"sessionUpdate": "tool_call",
            "toolCallId": format!("t{index}"), "title": "Run", "status": "completed",
            "rawOutput": "o".repeat(6_000), "content": [{"type": "content",
            "content": {"type": "text", "text": "c".repeat(6_000)}}]})));
    }
    assert!(recorder.bytes <= HARD_RECORDED_BYTES);
    let turn = recorder.finish().unwrap();
    assert_eq!(turn.updates.len() as u64 + turn.omitted, 1000);
    let last = turn.updates.last().unwrap();
    assert_eq!(
        last["toolCallId"], "t999",
        "late tool updates keep a skeleton"
    );
    assert!(last.get("rawOutput").is_none());
}

/// Over budget, the oldest tool outputs go first and the newest stay whole;
/// every part fits 60 KiB, there are at most 8, the skeleton fields survive,
/// and the parts concatenate back to every update in order.
/// Mutation: elide newest-first, or skip the skeleton, and the assertions on
/// the first and last updates fail.
#[test]
fn parts_fit_the_budget_by_eliding_the_oldest_tool_outputs() {
    let mut updates = Vec::new();
    for index in 0..120 {
        updates.push(json!({"sessionUpdate": "tool_call", "toolCallId": format!("t{index}"),
            "title": format!("Run {index}"), "kind": "execute", "status": "completed",
            "locations": [{"path": format!("/home/agent/repo/{index}.rs")}],
            "rawInput": {"command": format!("cargo test {index}")},
            "rawOutput": "o".repeat(6_000),
            "content": [{"type": "content", "content": {"type": "text", "text": "c".repeat(6_000)}}]}));
        updates.push(json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": format!("step {index}")}}));
    }
    let parts = split_into_parts(&header(), updates.clone(), 0);
    assert!(parts.len() <= MAX_PARTS && parts.len() > 1);
    for part in &parts {
        part.validate().unwrap();
        assert!(serde_json::to_string(part).unwrap().len() <= MAX_PART_PLAINTEXT_BYTES);
        assert_eq!(part.omitted, 0);
    }
    let joined: Vec<Value> = parts.iter().flat_map(|p| p.updates.clone()).collect();
    assert_eq!(joined.len(), updates.len(), "no update is dropped");
    for (kept, original) in joined.iter().zip(&updates) {
        for field in TOOL_SKELETON_FIELDS {
            assert_eq!(kept.get(field), original.get(field), "{field} is kept");
        }
    }
    assert!(
        joined[0]["rawOutput"]
            .as_str()
            .unwrap()
            .starts_with("…[elided "),
        "the oldest output is elided"
    );
    assert_eq!(
        joined[joined.len() - 2]["rawOutput"],
        updates[updates.len() - 2]["rawOutput"],
        "the newest output is whole"
    );
    assert_eq!(
        joined[0]["rawInput"], updates[0]["rawInput"],
        "inputs go only after every output"
    );

    // A small turn is one part, unchanged.
    let small = split_into_parts(&header(), updates[..2].to_vec(), 0);
    assert_eq!(small.len(), 1);
    assert_eq!(small[0].updates, updates[..2]);

    // A turn with nothing to say is still one (empty) part.
    assert_eq!(split_into_parts(&header(), Vec::new(), 3)[0].omitted, 3);

    // Adapter `_meta` (here claude-agent-acp's tool response) is elided like
    // an output, oldest first, rather than costing whole items.
    // Mutation: leave `_meta` out of both passes and items are dropped.
    let metas: Vec<Value> = (0..200)
        .map(|index| {
            json!({"sessionUpdate": "tool_call_update", "toolCallId": format!("m{index}"),
                "status": "completed",
                "_meta": {"claudeCode": {"toolResponse": "r".repeat(3_000)}}})
        })
        .collect();
    let parts = split_into_parts(&header(), metas.clone(), 0);
    let joined: Vec<Value> = parts.iter().flat_map(|p| p.updates.clone()).collect();
    assert_eq!(parts[0].omitted, 0);
    assert_eq!(joined.len(), metas.len());
    assert!(joined[0]["_meta"]
        .as_str()
        .is_some_and(|m| m.starts_with("…[elided ")));
    assert_eq!(joined[199]["_meta"], metas[199]["_meta"]);
}

/// One item too large for a part (a tool update with a huge `_meta` or
/// `locations`, an oversized plan, or a chunk with a huge `_meta`) is shrunk
/// or dropped on its own, both while recording and when splitting: it never
/// evicts the turn's message text, and never spends the recording budget.
/// Mutations: skip the per-item step in `split_into_parts` → the message text
/// is dropped; skip `shrink_item` in `record_update` (or the chunk skeleton
/// in `record_chunk`) → the recorder holds the whole oversized item; leave
/// `_meta` unshrinkable → the first row fails; stop bounding the skeleton's
/// `locations` → the second row fails.
#[test]
fn one_oversized_item_never_evicts_the_turn() {
    let paths: Vec<String> = (0..5_000)
        .map(|index| format!("/home/agent/repo/src/f{index:04}.rs"))
        .collect();
    let says = |id: &str, text: &str| {
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id,
               "content": {"type": "text", "text": text}})
    };
    type Check = fn(&Value);
    let rows: [(&str, Value, u64, Check); 4] = [
        (
            "tool _meta holding 5,000 paths",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "glob",
                "title": "Glob", "kind": "search", "status": "completed",
                "_meta": {"claudeCode": {"toolResponse": {"filenames": paths}}}}),
            0,
            |item| {
                assert_eq!(item["toolCallId"], "glob");
                assert_eq!(item["title"], "Glob");
                assert!(item["_meta"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("…[elided ")));
            },
        ),
        (
            "tool with 5,000 locations",
            json!({"sessionUpdate": "tool_call", "toolCallId": "grep", "title": "Grep",
                "kind": "search", "status": "completed",
                "locations": paths.iter().map(|path| json!({"path": path})).collect::<Vec<_>>()}),
            0,
            |item| {
                assert_eq!(item["toolCallId"], "grep");
                assert_eq!(
                    item["locations"].as_array().unwrap().len(),
                    MAX_SKELETON_LOCATIONS
                );
                assert_eq!(
                    item["locations"][0]["path"],
                    "/home/agent/repo/src/f0000.rs"
                );
            },
        ),
        (
            "plan with 5,000 entries",
            json!({"sessionUpdate": "plan",
                "entries": paths.iter().map(|path| json!({"content": path})).collect::<Vec<_>>()}),
            1,
            |_| unreachable!("an unshrinkable plan is dropped"),
        ),
        (
            "chunk with a huge _meta",
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "big",
                "content": {"type": "text", "text": "big meta"},
                "_meta": {"claudeCode": {"files": paths}}}),
            0,
            |item| {
                assert_eq!(item["content"]["text"], "big meta");
                assert!(item.get("_meta").is_none());
            },
        ),
    ];
    for (label, oversized, omitted, check) in rows {
        assert!(json_len(&oversized) > MAX_PART_PLAINTEXT_BYTES, "{label}");
        let updates = vec![says("a", "before"), oversized, says("b", "after")];
        let assert_kept = |updates: &[Value], omitted_now: u64| {
            assert_eq!(omitted_now, omitted, "{label}: omitted");
            assert_eq!(
                updates.first().unwrap()["content"]["text"],
                "before",
                "{label}"
            );
            assert_eq!(
                updates.last().unwrap()["content"]["text"],
                "after",
                "{label}"
            );
            assert_eq!(updates.len() as u64, 3 - omitted, "{label}");
            if omitted == 0 {
                check(&updates[1]);
            }
        };

        // Splitting a turn that holds the item whole.
        let parts = split_into_parts(&header(), updates.clone(), 0);
        for part in &parts {
            assert!(serde_json::to_string(part).unwrap().len() <= MAX_PART_PLAINTEXT_BYTES);
        }
        let joined: Vec<Value> = parts.iter().flat_map(|p| p.updates.clone()).collect();
        assert_kept(&joined, parts[0].omitted);

        // Recording the item: it is shrunk (or dropped) before it is kept.
        let mut recorder = TurnRecorder::default();
        recorder.record_write(&prompt(1));
        for item in updates {
            recorder.record_read(&update(item));
        }
        assert!(
            recorder.bytes < ITEM_LIMIT_BYTES,
            "{label}: recorder holds {} bytes",
            recorder.bytes
        );
        let turn = recorder.finish().unwrap();
        let parts = split_into_parts(&header(), turn.updates, turn.omitted);
        let joined: Vec<Value> = parts.iter().flat_map(|p| p.updates.clone()).collect();
        assert_kept(&joined, parts[0].omitted);
    }
}

/// A field is replaced by its elision marker only when that makes it
/// smaller, so shrinking can never grow an update.
/// Mutation: drop the size comparison → the short rows are replaced.
#[test]
fn eliding_a_field_never_grows_it() {
    for (field, value, shrinks) in [
        ("rawOutput", json!("ok"), false),
        ("rawOutput", json!({"exitCode": 0}), false),
        ("content", json!([]), false),
        (
            "content",
            json!([{"type": "content", "content": {"type": "text", "text": "hi"}}]),
            false,
        ),
        ("rawInput", json!({"command": "x".repeat(200)}), true),
        (
            "content",
            json!([{"type": "content", "content": {"type": "text", "text": "y".repeat(200)}}]),
            true,
        ),
    ] {
        let mut update = json!({"sessionUpdate": "tool_call", "toolCallId": "t"});
        update[field] = value.clone();
        let before = json_len(&update);
        assert_eq!(
            elide_tool_field(&mut update, field),
            shrinks,
            "{field}: {value}"
        );
        if shrinks {
            assert!(json_len(&update) < before, "{field}: {value}");
            assert!(is_elided_field(&update[field]), "{field}: {value}");
        } else {
            assert_eq!(update[field], value, "{field}: {value}");
        }
    }
}

/// Exact values of known secret variables, and the agent key in hex and
/// `nsec`, never reach a recorded update: not in a tool's title, input, or
/// output, not in `_meta`, and not in chunk text that splits a secret across
/// chunks or across a compaction. Short values and non-secret variables are
/// left alone.
/// Mutations: skip `redact` in `record_update` → the tool row leaks; skip
/// `redact_appended` in `RollingText::push` → the split token leaks; start
/// its scan at `appended` → the split token leaks.
#[test]
fn known_secret_values_are_redacted_before_recording() {
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let hex = keys.secret_key().to_secret_hex();
    let token = "ghp_0123456789abcdefTRANSCRIPTsecret";
    let api_key = "sk-ant-api03-transcript-secret-value";
    let secrets = Secrets::collect(
        Some(&keys),
        [
            ("GITHUB_TOKEN", token),
            ("ANTHROPIC_API_KEY", api_key),
            ("BUZZ_PRIVATE_KEY", nsec.as_str()),
            ("SHORT_TOKEN", "1"),
            ("HOME", "/home/agent/workspace"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
    let mut recorder = TurnRecorder::new(Arc::new(secrets));
    recorder.record_write(&prompt(1));
    recorder.record_read(&update(
        json!({"sessionUpdate": "tool_call", "toolCallId": "t1",
        "title": format!("git clone https://x-access-token:{token}@github.com/acme/w"),
        "status": "completed",
        "rawInput": {"command": format!("buzz --key {nsec} send; echo {hex}")},
        "rawOutput": format!("{}{api_key}", "o".repeat(LEAF_LIMIT_BYTES * 2)),
        "_meta": {"env": format!("ANTHROPIC_API_KEY={api_key} HOME=/home/agent/workspace 1")}}),
    ));
    // A token split across two chunks, then one cut by compaction.
    let (head, tail) = token.split_at(10);
    for piece in [format!("token is {head}"), format!("{tail}.")] {
        recorder.record_read(&chunk("agent_message_chunk", &piece));
    }
    for _ in 0..40 {
        recorder.record_read(&chunk("agent_message_chunk", &"n".repeat(1_000)));
        recorder.record_read(&chunk("agent_message_chunk", head));
        recorder.record_read(&chunk("agent_message_chunk", tail));
    }
    let turn = recorder.finish().unwrap();
    let recorded = serde_json::to_string(&turn.updates).unwrap();
    for secret in [token, api_key, nsec.as_str(), hex.as_str(), head] {
        assert!(!recorded.contains(secret), "leaked {secret}");
    }
    assert!(recorded.contains(secrets::REDACTED));
    assert!(
        recorded.contains("/home/agent/workspace 1"),
        "non-secrets stay"
    );
    assert_eq!(
        turn.updates[1]["content"]["text"]
            .as_str()
            .unwrap()
            .get(..20),
        Some("token is [redacted].")
    );
}

/// A tiny HTTP relay for `POST /events`. The first attempt for each event
/// fails with HTTP 500 when `fail_first`; submissions for `slow` answer after
/// one second. It records each stored part as (recipient, part index).
async fn publish_relay(
    fail_first: bool,
    slow: Option<PublicKey>,
) -> (String, Arc<Mutex<Vec<(PublicKey, usize)>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let stored = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let stored_by_server = Arc::clone(&stored);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (stored, attempts) = (Arc::clone(&stored_by_server), Arc::clone(&attempts));
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 8192];
                let body = loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break body.to_owned();
                    }
                };
                let event: nostr::Event = serde_json::from_str(&body).unwrap();
                let envelope = buzz_sdk::transcript::parse_envelope(&event).unwrap();
                let first = attempts.lock().unwrap().insert(event.id);
                let status = if fail_first && first {
                    "500 Internal Server Error"
                } else {
                    if slow == Some(envelope.recipient) {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    stored
                        .lock()
                        .unwrap()
                        .push((envelope.recipient, envelope.part_index));
                    "200 OK"
                };
                let payload = r#"{"accepted":true,"message":""}"#;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (base_url, stored)
}

/// Each recipient gets every part in order, a failed submission is retried,
/// and a slow recipient does not hold up the others.
/// Mutations: one attempt per part → the retry run stores nothing; deliver
/// recipients one after another → the fast recipient's parts land after the
/// slow recipient's.
#[tokio::test]
async fn delivery_retries_and_does_not_let_one_recipient_starve_another() {
    let agent = nostr::Keys::generate();
    let (owner, gateway) = (
        nostr::Keys::generate().public_key(),
        nostr::Keys::generate().public_key(),
    );
    let parts: Vec<TranscriptPayload> = (0..3)
        .map(|index| TranscriptPayload {
            updates: vec![json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": format!("part {index}")}})],
            ..header()
        })
        .collect();
    let meta = TurnMeta {
        turn_id: "turn-1".into(),
        turn_seq: 1,
        thread_root: EventId::from_hex(&"ab".repeat(32)).unwrap(),
        channel: uuid::Uuid::new_v4(),
        started_at: "2026-11-13T10:00:00Z".into(),
    };
    for (fail_first, slow) in [(true, None), (false, Some(owner))] {
        let (base_url, stored) = publish_relay(fail_first, slow).await;
        let sink = Sink {
            keys: agent.clone(),
            owner,
            gateways: None,
            rest: RestClient {
                http: reqwest::Client::new(),
                base_url,
                keys: agent.clone(),
                auth_tag_json: None,
            },
            cwd: PathBuf::from("."),
            secrets: Arc::default(),
            tasks: TaskTracker::new(),
        };
        tokio::time::timeout(
            Duration::from_secs(20),
            deliver(&sink, &meta, &parts, &[owner, gateway]),
        )
        .await
        .expect("delivery is bounded");
        let stored = stored.lock().unwrap().clone();
        for recipient in [owner, gateway] {
            let indices: Vec<usize> = stored
                .iter()
                .filter(|(to, _)| *to == recipient)
                .map(|(_, index)| *index)
                .collect();
            assert_eq!(indices, [0, 1, 2], "fail_first={fail_first}: in order");
        }
        if slow.is_some() {
            let first_slow = stored.iter().position(|(to, _)| *to == owner).unwrap();
            let last_fast = stored.iter().rposition(|(to, _)| *to == gateway).unwrap();
            assert!(
                last_fast < first_slow,
                "the gateway's parts land while the owner's are slow: {stored:?}"
            );
        }
    }
}

/// When even skeletons do not fit, the oldest chunks and plans are dropped
/// before any tool update, and the drop is counted, never silent.
#[test]
fn parts_drop_oldest_non_tool_updates_last_and_count_them() {
    let mut updates = Vec::new();
    for index in 0..1_600 {
        updates.push(json!({"sessionUpdate": "plan",
            "entries": [{"content": format!("{index}:{}", "p".repeat(400))}]}));
    }
    updates.push(json!({"sessionUpdate": "tool_call", "toolCallId": "keep",
        "title": "Edit", "status": "completed"}));
    let parts = split_into_parts(&header(), updates.clone(), 2);
    assert!(parts.len() == MAX_PARTS);
    let joined: Vec<Value> = parts.iter().flat_map(|p| p.updates.clone()).collect();
    let dropped = updates.len() - joined.len();
    assert!(dropped > 0);
    assert!(parts.iter().all(|p| p.omitted == 2 + dropped as u64));
    assert_eq!(joined.last().unwrap()["toolCallId"], "keep");
    assert_eq!(joined[0], updates[dropped], "the oldest plans went first");
    for part in &parts {
        assert!(serde_json::to_string(part).unwrap().len() <= MAX_PART_PLAINTEXT_BYTES);
    }
}

#[test]
fn github_remotes_map_to_owner_repo_and_others_do_not() {
    for (url, repo) in [
        ("https://github.com/acme/widgets.git", Some("acme/widgets")),
        ("https://github.com/acme/widgets", Some("acme/widgets")),
        (
            "https://x-access-token:secret@github.com/acme/widgets.git",
            Some("acme/widgets"),
        ),
        ("git@github.com:acme/widgets.git", Some("acme/widgets")),
        (
            "ssh://git@github.com/acme/widgets.git",
            Some("acme/widgets"),
        ),
        (
            "ssh://git@github.com:22/acme/w.dots_ok-1",
            Some("acme/w.dots_ok-1"),
        ),
        ("https://gitlab.com/acme/widgets.git", None),
        ("https://github.com.evil.example/acme/widgets", None),
        ("https://github.com/acme", None),
        ("https://github.com/acme/widgets/tree/main", None),
        ("/srv/git/widgets.git", None),
    ] {
        assert_eq!(github_repo(url).as_deref(), repo, "{url}");
    }
}

fn git_ok(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn checkout(root: &Path, name: &str, remote: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir(&dir).unwrap();
    git_ok(&dir, &["init", "-q", "-b", "main"]);
    git_ok(&dir, &["remote", "add", "origin", remote]);
    git_ok(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    dir
}

/// Workspace detection reads GitHub checkouts directly under the working
/// directory from local refs: `pushed` follows `origin/<branch>`, a detached
/// HEAD has no branch, and non-GitHub or non-git directories are left out.
/// Mutation: drop the `merge-base --is-ancestor` check (e.g. always true)
/// and the unpushed row fails.
#[tokio::test]
async fn workspace_reads_branch_head_and_pushed_from_local_refs() {
    let root = tempfile::tempdir().unwrap();
    let pushed = checkout(
        root.path(),
        "a-pushed",
        "https://github.com/acme/pushed.git",
    );
    git_ok(&pushed, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let ahead = checkout(root.path(), "b-ahead", "git@github.com:acme/ahead.git");
    git_ok(&ahead, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git_ok(&ahead, &["commit", "-q", "--allow-empty", "-m", "two"]);
    let detached = checkout(
        root.path(),
        "c-detached",
        "https://github.com/acme/detached",
    );
    git_ok(&detached, &["checkout", "-q", "--detach"]);
    checkout(root.path(), "d-gitlab", "https://gitlab.com/acme/other.git");
    std::fs::create_dir(root.path().join("e-plain")).unwrap();

    let entries = detect_workspace(root.path()).await;
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let summary: Vec<(String, String, Option<String>, bool)> = entries
        .iter()
        .map(|e| (e.path.clone(), e.repo.clone(), e.branch.clone(), e.pushed))
        .collect();
    let path = |name: &str| canonical.join(name).to_string_lossy().into_owned();
    assert_eq!(
        summary,
        vec![
            (
                path("a-pushed"),
                "acme/pushed".into(),
                Some("main".into()),
                true
            ),
            (
                path("b-ahead"),
                "acme/ahead".into(),
                Some("main".into()),
                false
            ),
            (path("c-detached"), "acme/detached".into(), None, false),
        ]
    );
    assert!(entries.iter().all(|e| e.head.len() == 40));

    // A checkout as the working directory itself counts too.
    let own = detect_workspace(&pushed).await;
    assert_eq!(own.len(), 1);
    assert!(own[0].pushed);
}
