use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use nostr::{Event, Timestamp};
use std::sync::{Arc, Mutex};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

fn snapshot() -> String {
    let avatar = format!("data:image/png;base64,{}", STANDARD.encode(PNG));
    json!({
        "format": "buzz-team-snapshot", "version": 1,
        "team": {"name": "Asgard", "description": "d", "instructions": "Ship carefully."},
        "members": [
            {"format": "buzz-agent-snapshot", "version": 1,
             "definition": {"name": "Frigg", "runtime": "codex-voltage", "systemPrompt": "You are Frigg."},
             "profile": {"displayName": "Frigg", "about": "Manager.", "avatarDataUrl": avatar}},
            {"format": "buzz-agent-snapshot", "version": 1,
             "definition": {"name": "Tyr", "runtime": "claude", "systemPrompt": "You are Tyr."},
             "profile": {"displayName": "Tyr"}},
        ],
    })
    .to_string()
}

#[test]
fn snapshot_parsing_and_harness_mapping() {
    let team = parse_snapshot(&snapshot()).unwrap();
    assert_eq!(team.name, "Asgard");
    assert_eq!(team.members[0].avatar.as_deref(), Some(PNG));
    assert_eq!(team.members[1].about, None);
    for (runtime, expected) in [
        ("codex-voltage", "codex"),
        ("codex", "codex"),
        ("claude", "claude"),
        ("buzz-agent", "buzz-agent"),
        ("goose", "claude"),
        ("", "claude"),
    ] {
        assert_eq!(harness(runtime).id, expected, "{runtime}");
    }
    let mut value: Value = serde_json::from_str(&snapshot()).unwrap();
    type Break = fn(&mut Value);
    let rows: [(&str, Break); 5] = [
        ("format", |v| v["format"] = json!("buzz-agent-snapshot")),
        ("no members", |v| v["members"] = json!([])),
        ("duplicate names", |v| {
            v["members"][1]["profile"]["displayName"] = json!("frigg")
        }),
        // Sorted case-sensitively, "Frigg" and "frigg" are not neighbors.
        ("case variant apart", |v| {
            let mut copy = v["members"][0].clone();
            copy["profile"]["displayName"] = json!("frigg");
            v["members"].as_array_mut().unwrap().push(copy);
        }),
        ("avatar", |v| {
            v["members"][0]["profile"]["avatarDataUrl"] = json!("https://x/y.png")
        }),
    ];
    for (row, break_it) in rows {
        let mut broken = value.clone();
        break_it(&mut broken);
        assert!(parse_snapshot(&broken.to_string()).is_err(), "{row}");
    }
    value["members"][0]["profile"]
        .as_object_mut()
        .unwrap()
        .remove("displayName");
    assert_eq!(
        parse_snapshot(&value.to_string()).unwrap().members[0].name,
        "Frigg"
    );
}

#[test]
fn launch_and_team_prompt_match_the_app() {
    let team = parse_snapshot(&snapshot()).unwrap();
    let (frigg, tyr) = (Keys::generate().public_key(), Keys::generate().public_key());
    let roster = vec![("Frigg".to_owned(), frigg), ("Tyr".to_owned(), tyr)];
    let prompt = team_prompt(&team, &roster, &tyr);
    let lines: Vec<&str> = prompt.lines().collect();
    assert_eq!(
        lines[1],
        "You are on the team \"Asgard\". Its members, you included:"
    );
    assert!(lines[2].starts_with("- Frigg (npub1") && !lines[2].ends_with(", you"));
    assert!(lines[3].starts_with("- Tyr (npub1") && lines[3].ends_with(", you"));
    assert_eq!(
        &lines[5..8],
        [
            "<team-instructions>",
            "Ship carefully.",
            "</team-instructions>"
        ]
    );

    let owner = Keys::generate().public_key();
    let launch = launch(
        &team.members[0],
        CODEX,
        Some("gpt-6-astra"),
        &prompt,
        &owner,
    );
    assert_eq!(launch["command"], "codex-acp");
    assert_eq!(launch["owner_pubkey"], owner.to_hex());
    assert_eq!(launch["env"], json!({}));
    assert_eq!(
        launch["policy_env"],
        json!({
            "BUZZ_ACP_AGENTS": "1",
            "BUZZ_ACP_DISPLAY_NAME": "Frigg",
            "BUZZ_ACP_LAZY_POOL": "true",
            "BUZZ_ACP_MODEL": "gpt-6-astra",
            "BUZZ_ACP_RELAY_OBSERVER": "true",
            "BUZZ_ACP_SESSION_POLICY": "thread",
            "BUZZ_ACP_SESSION_TITLE": "Frigg",
            "BUZZ_ACP_SYSTEM_PROMPT": "You are Frigg.",
            "BUZZ_ACP_TEAM_INSTRUCTIONS": prompt,
        })
    );
    let claude = super::launch(
        &team.members[1],
        CLAUDE,
        Some("claude-opus-5-5"),
        "",
        &owner,
    );
    assert_eq!(claude["policy_env"]["ANTHROPIC_MODEL"], "claude-opus-5-5");
    assert!(claude["policy_env"]
        .get("BUZZ_ACP_TEAM_INSTRUCTIONS")
        .is_none());
}

/// The relay, its Blossom endpoint and the Sandbox manager behind it.
#[derive(Default)]
struct Fake {
    manager: Option<Keys>,
    events: Vec<Event>,
    deploys: Vec<(Event, Value)>,
    /// Agent names whose deploy the manager refuses.
    refuse: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

async fn socket(fake: Shared, mut socket: WebSocket) {
    let send = |text: Value| Message::Text(text.to_string().into());
    socket
        .send(send(json!(["AUTH", "challenge"])))
        .await
        .unwrap();
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).unwrap();
        match frame[0].as_str().unwrap() {
            "AUTH" => {
                let auth: Event = serde_json::from_value(frame[1].clone()).unwrap();
                socket
                    .send(send(json!(["OK", auth.id, true, ""])))
                    .await
                    .unwrap();
            }
            "REQ" => socket.send(send(json!(["EOSE", frame[1]]))).await.unwrap(),
            "EVENT" => {
                let event: Event = serde_json::from_value(frame[1].clone()).unwrap();
                socket
                    .send(send(json!(["OK", event.id, true, ""])))
                    .await
                    .unwrap();
                let reply = {
                    let mut fake = fake.lock().unwrap();
                    let manager = fake.manager.clone().unwrap();
                    let opened = buzz_session_control::open_request(&event, &manager).unwrap();
                    assert_eq!(opened.request.op, Op::Deploy);
                    let payload = opened.request.payload.clone();
                    let name = payload["agent"]["name"].as_str().unwrap().to_owned();
                    let response = if fake.refuse.contains(&name) {
                        json!({"ok": false, "code": "capacity", "error": "no capacity"})
                    } else {
                        json!({"ok": true})
                    };
                    fake.deploys.push((event.clone(), payload));
                    buzz_session_control::build_reply(
                        &manager,
                        &opened.signer,
                        &event.id,
                        &buzz_session_control::Reply::done(opened.request.request_id, response),
                        Timestamp::now(),
                    )
                    .unwrap()
                };
                socket
                    .send(send(json!(["EVENT", "paseo-pairing", reply])))
                    .await
                    .unwrap();
            }
            other => panic!("unexpected frame {other}"),
        }
    }
}

async fn serve(fake: Shared) -> String {
    let app = Router::new()
        .route(
            "/",
            get(
                |State(fake): State<Shared>, upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |ws| socket(fake, ws))
                },
            ),
        )
        // Channel metadata exists once a create command was accepted.
        .route(
            "/query",
            post(
                |State(fake): State<Shared>, Json(filters): Json<Value>| async move {
                    let wanted = filters[0]["#d"][0].as_str().unwrap_or_default().to_owned();
                    let fake = fake.lock().unwrap();
                    let exists = fake.events.iter().any(|event| {
                        event.kind.as_u16() == 9007
                            && tag_value(event, "h") == Some(wanted.as_str())
                    });
                    Json(if exists {
                        json!([{"kind": 39000, "tags": [["d", wanted]]}])
                    } else {
                        json!([])
                    })
                },
            ),
        )
        .route(
            "/events",
            post(
                |State(fake): State<Shared>, Json(event): Json<Event>| async move {
                    event.verify().unwrap();
                    let id = event.id;
                    fake.lock().unwrap().events.push(event);
                    Json(json!({"event_id": id, "accepted": true, "message": ""}))
                },
            ),
        )
        .route(
            "/upload",
            put(|| async {
                Json(
                    json!({"url": "https://blossom.example/avatar.png", "sha256": "00",
                    "size": PNG.len(), "type": "image/png", "uploaded": 0}),
                )
            }),
        )
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address.to_string()
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [key, value, ..] if key == name => Some(value.as_str()),
        _ => None,
    })
}

#[tokio::test]
async fn import_creates_lists_seats_and_deploys_each_agent_and_resumes() {
    let owner = Keys::generate();
    let manager_keys = Keys::generate();
    let fake = Arc::new(Mutex::new(Fake {
        manager: Some(manager_keys.clone()),
        refuse: vec!["Tyr".into()],
        ..Fake::default()
    }));
    let address = serve(fake.clone()).await;
    let client = BuzzClient::new(format!("http://{address}"), owner.clone(), None, None).unwrap();
    let manager = Manager {
        relay: format!("ws://{address}"),
        owner: owner.clone(),
        manager: manager_keys.public_key(),
    };
    let journal_dir = tempfile::tempdir().unwrap();
    let options = || Options {
        channel: None,
        model: None,
        journal_dir: journal_dir.path().to_owned(),
    };
    let team = parse_snapshot(&snapshot()).unwrap();

    // First run: Tyr's deploy is refused; everything else completes.
    let first = import(&client, &manager, &team, options()).await.unwrap();
    assert_eq!(first["failed"][0]["name"], "Tyr");
    let error = outcome(&first).unwrap_err();
    assert!(
        error.to_string().contains("1 agent(s) did not deploy"),
        "{error}"
    );
    let path = Journal::path(
        journal_dir.path(),
        "Asgard",
        client.relay_url(),
        &owner.public_key().to_hex(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the journal holds agent keys");
    }
    let journal = Journal::load(&path).unwrap().unwrap();
    let frigg = &journal.members[0];
    assert!(frigg.profile && frigg.listed && frigg.joined && frigg.deployed);
    assert!(!journal.members[1].deployed);
    let channel = journal.channel_id.unwrap().to_string();
    {
        let fake = fake.lock().unwrap();
        let by_kind = |kind: u16| fake.events.iter().filter(move |e| e.kind.as_u16() == kind);
        // Profiles: signed by each agent, attested, with the uploaded avatar.
        let profiles: Vec<&Event> = by_kind(0).collect();
        assert_eq!(profiles.len(), 2);
        let content: Value = serde_json::from_str(&profiles[0].content).unwrap();
        assert_eq!(profiles[0].pubkey.to_hex(), frigg.pubkey);
        assert_eq!(content["bot"], true);
        assert_eq!(content["picture"], "https://blossom.example/avatar.png");
        assert!(
            tag_value(profiles[0], "auth").is_some(),
            "the profile carries the attestation"
        );
        // Listings: owner-signed, one per agent.
        let listing = by_kind(LISTING_KIND).next().unwrap();
        assert_eq!(listing.pubkey, owner.public_key());
        assert_eq!(tag_value(listing, "d"), Some(frigg.pubkey.as_str()));
        assert_eq!(tag_value(listing, "runtime"), Some(REMOTE_RUNTIME));
        // The private team channel and both seats.
        let create = by_kind(9007).next().unwrap();
        assert_eq!(tag_value(create, "name"), Some("Asgard"));
        assert_eq!(tag_value(create, "visibility"), Some("private"));
        assert_eq!(by_kind(9000).count(), 2);
        // Frigg's deploy: signed as Frigg, bound to its welcome thread.
        let (event, payload) = &fake.deploys[0];
        assert_eq!(event.pubkey.to_hex(), frigg.pubkey);
        let welcome = by_kind(9)
            .find(|e| tag_value(e, "p") == Some(frigg.pubkey.as_str()))
            .unwrap();
        assert_eq!(
            payload["provider_config"]["sandbox"],
            json!({"channel_id": channel, "thread_root": welcome.id.to_hex()})
        );
        assert_eq!(
            payload["provider_config"]["identity_policy"]["owner_pubkey"],
            owner.public_key().to_hex()
        );
        assert_eq!(payload["agent"]["auth_tag"], frigg.auth_tag);
        assert_eq!(payload["agent"]["launch"]["command"], "codex-acp");
        assert_eq!(
            payload["agent"]["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"],
            welcome.created_at.as_secs().to_string()
        );
    }

    // Rerun: only Tyr is retried, with its first identity and welcome
    // thread, so a retry never starts a second thread.
    // Nothing is republished for Frigg and no second channel is created.
    let (first_tyr, first_thread, first_request) = {
        let fake = fake.lock().unwrap();
        let (event, payload) = &fake.deploys[1];
        let opened = buzz_session_control::open_request(event, &manager_keys).unwrap();
        (
            event.pubkey,
            payload["provider_config"]["sandbox"]["thread_root"].clone(),
            opened.request.request_id,
        )
    };
    fake.lock().unwrap().refuse.clear();
    let events_before = fake.lock().unwrap().events.len();
    let report = import(&client, &manager, &team, options()).await.unwrap();
    assert_eq!(report["agents"][1]["deployed"], true);
    assert!(outcome(&report).is_ok());
    let journal = Journal::load(&path).unwrap().unwrap();
    assert_eq!(journal.members[1].pubkey, report["agents"][1]["pubkey"]);
    let shared = fake.clone();
    {
        let fake = fake.lock().unwrap();
        let new: Vec<u16> = fake.events[events_before..]
            .iter()
            .map(|e| e.kind.as_u16())
            .collect();
        assert_eq!(new, [9], "only Tyr's saved welcome, resubmitted");
        assert_eq!(fake.deploys.len(), 3);
        let (retry, payload) = &fake.deploys[2];
        assert_eq!(retry.pubkey, first_tyr, "the retry keeps Tyr's first key");
        assert_eq!(
            payload["provider_config"]["sandbox"]["thread_root"],
            first_thread
        );
        // The first attempt was a definite refusal that started nothing, so the
        // retry carries a new request ID; the manager would replay the refusal.
        let opened = buzz_session_control::open_request(retry, &manager_keys).unwrap();
        assert_ne!(opened.request.request_id, first_request);
        assert_eq!(
            fake.events[events_before].id.to_hex(),
            first_thread.as_str().unwrap()
        );
    }

    // A create command whose reply was lost: the journal holds the channel
    // ID but not its creation. A rerun finds the channel and creates none.
    let mut journal = Journal::load(&path).unwrap().unwrap();
    journal.channel_created = false;
    journal.save(&path).unwrap();
    let creates = |fake: &Shared| {
        fake.lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind.as_u16() == 9007)
            .count()
    };
    assert_eq!(creates(&shared), 1);
    import(&client, &manager, &team, options()).await.unwrap();
    assert_eq!(
        creates(&shared),
        1,
        "an existing channel is not created again"
    );
    assert!(Journal::load(&path).unwrap().unwrap().channel_created);
}

#[test]
fn one_import_holds_the_journal_lock_at_a_time() {
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("asgard.json");
    let held = JournalLock::acquire(&journal).unwrap();
    assert!(matches!(
        JournalLock::acquire(&journal),
        Err(CliError::Conflict(_))
    ));
    drop(held);
    assert!(JournalLock::acquire(&journal).is_ok());
}
