//! Channel usage over saved turn metrics addressed to the test relay's
//! alice. Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`.
//!
//! Agents send turn metrics only over an owner-attested WebSocket route,
//! so throwaway agent keys sign the metrics here and they go straight
//! into a temporary archive, as capture would save them. Nothing is sent
//! to the relay. With `BUZZ_USAGE_SEED_ARCHIVE` set to an app archive
//! file, the same metrics are written there instead (for screenshots).

use buzz_identity::Identity;
use buzz_relay_client::RelayOrigin;
use buzz_relay_client::archive::{self, Archive};
use buzz_relay_client::usage::{self, SavedMetrics, Scan};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp, nip44};
use serde_json::{Value, json};
use std::sync::Arc;

fn envelope(agent: &Keys, viewer: &Identity, frame: &Value, at: u64) -> Event {
    let content = nip44::encrypt(
        agent.secret_key(),
        &viewer.public_key(),
        frame.to_string(),
        nip44::Version::V2,
    )
    .unwrap();
    EventBuilder::new(Kind::from_u16(44200), content)
        .tags([
            Tag::parse(["p", &viewer.pubkey_hex()]).unwrap(),
            Tag::parse(["agent", &agent.public_key().to_hex()]).unwrap(),
        ])
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(agent)
        .unwrap()
}

fn iso(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .unwrap()
        .to_rfc3339()
}

/// One turn's metrics: this turn and the session's running totals.
#[allow(clippy::too_many_arguments)]
fn turn(
    channel: &str,
    session: &str,
    seq: u64,
    model: &str,
    at: u64,
    input: u64,
    output: u64,
    cache: u64,
    cost: f64,
    running: (u64, u64, u64, f64),
) -> Value {
    json!({
        "harness": "goose",
        "model": model,
        "timestamp": iso(at),
        "channelId": channel,
        "sessionId": session,
        "turnId": if seq == 0 { format!("{session}:initial") } else { format!("{session}:{seq}") },
        "turnSeq": seq,
        "stopReason": "end_turn",
        "pricingIdentity": {"authority": "models.dev", "model": model},
        "turn": {
            "inputTokens": input, "outputTokens": output, "totalTokens": input + output,
            "cacheReadTokens": cache, "cacheWriteTokens": 0, "costUsd": cost,
        },
        "cumulative": {
            "inputTokens": running.0, "outputTokens": running.1,
            "totalTokens": running.0 + running.1, "cacheReadTokens": running.2,
            "cacheWriteTokens": 0, "costUsd": running.3,
        },
    })
}

#[test]
fn alice_reads_her_agents_usage_for_a_channel() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let read = |file: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{file}")).unwrap()).unwrap()
    };
    let (identities, seed) = (read("identities.json"), read("seed.json"));
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let general = seed["channels"]["general"].as_str().unwrap();
    let random = seed["channels"]["random"].as_str().unwrap();
    let community = archive::community_key(&origin);

    let temp = tempfile::tempdir().unwrap();
    let path = std::env::var("BUZZ_USAGE_SEED_ARCHIVE")
        .map(Into::into)
        .unwrap_or_else(|_| temp.path().join("events.sqlite3"));
    let archive = Arc::new(Archive::open(path).unwrap());
    let me = alice.pubkey_hex();
    let revision = archive.settings(&me, &community).unwrap().revision;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (builder, reviewer) = (Keys::generate(), Keys::generate());
    let mut frames = Vec::new();
    // The builder: two sessions in #general.
    let mut running = (0, 0, 0, 0.);
    for (seq, (input, output, cache, cost)) in [
        (4_200, 0, 0, 0.0126),
        (12_840, 1_536, 9_800, 0.0412),
        (18_302, 2_210, 15_004, 0.0587),
        (21_977, 3_408, 18_220, 0.0734),
    ]
    .into_iter()
    .enumerate()
    {
        running = (
            running.0 + input,
            running.1 + output,
            running.2 + cache,
            running.3 + cost,
        );
        let at = now - 3_600 + seq as u64 * 300;
        let frame = turn(
            general,
            "builder-a",
            seq as u64,
            "claude-sonnet-4-5",
            at,
            input,
            output,
            cache,
            cost,
            running,
        );
        frames.push((builder.clone(), frame, at));
    }
    let at = now - 600;
    let frame = turn(
        general,
        "builder-b",
        1,
        "claude-sonnet-4-5",
        at,
        2_048,
        512,
        0,
        0.0031,
        (2_048, 512, 0, 0.0031),
    );
    frames.push((builder.clone(), frame, at));
    // The reviewer: one session in #general, one in #random.
    let at = now - 1_200;
    let frame = turn(
        general,
        "reviewer-a",
        1,
        "gpt-5",
        at,
        9_120,
        880,
        6_000,
        0.0209,
        (9_120, 880, 6_000, 0.0209),
    );
    frames.push((reviewer.clone(), frame, at));
    let frame = turn(
        random,
        "reviewer-b",
        1,
        "gpt-5",
        at,
        100,
        10,
        0,
        0.0002,
        (100, 10, 0, 0.0002),
    );
    frames.push((reviewer.clone(), frame, at));
    for (agent, frame, at) in &frames {
        let event = envelope(agent, &alice, frame, *at);
        archive::check_envelope(&alice, &event, *at).unwrap();
        archive.ingest(&me, &community, &event, revision).unwrap();
    }

    let saved = SavedMetrics {
        archive,
        identity: alice,
        community,
    };
    let Scan::Ready {
        records,
        skipped,
        partial,
        ..
    } = usage::scan(&saved, || true)
    else {
        panic!("the saved metrics read back");
    };
    assert!(!partial);
    assert_eq!(skipped, 0);
    let projection = usage::project_usage(&records, general);
    assert_eq!(projection.unreadable, 0);
    let builder_sessions: Vec<_> = projection
        .groups
        .iter()
        .filter(|group| group.agent == builder.public_key().to_hex())
        .collect();
    assert_eq!(builder_sessions.len(), 2);
    let first = builder_sessions
        .iter()
        .find(|group| group.session_id.as_deref() == Some("builder-a"))
        .unwrap();
    assert_eq!(first.turns.len(), 4);
    assert_eq!(first.turns[3].turn_id.as_deref(), Some("builder-a:initial"));
    let totals = usage::aggregate_session_usage(
        &builder_sessions
            .iter()
            .map(|group| (*group).clone())
            .collect::<Vec<_>>(),
    );
    assert_eq!(totals.complete, 2);
    assert_eq!(totals.counters.input_tokens, Some(57_319 + 2_048));
    assert_eq!(totals.counters.cache_read_tokens, Some(43_024));
    assert!((totals.counters.cost_usd.unwrap() - 0.189).abs() < 1e-9);
    // The reviewer's #random session is not this channel's usage.
    let reviewer_sessions = projection
        .groups
        .iter()
        .filter(|group| group.agent == reviewer.public_key().to_hex())
        .count();
    assert_eq!(reviewer_sessions, 1);
}
