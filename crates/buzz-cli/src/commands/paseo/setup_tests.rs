use super::*;
use crate::commands::paseo::test_prompt::Script;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use buzz_sdk::gateway::Delegation;
use nostr::{Event, Keys, Timestamp};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The relay and the Sandbox manager behind it.
struct Fake {
    manager: Keys,
    gateway: Keys,
    /// The stored profile summary, or null.
    profile: Value,
    daemon: bool,
    refuse_enroll: bool,
    enrolls: Vec<Value>,
    grants: Vec<Event>,
}

type Shared = Arc<Mutex<Fake>>;

fn pairing_json(gateway: &Keys) -> Value {
    json!({
        "password": URL_SAFE_NO_PAD.encode([9u8; 32]),
        "server_id": "srv_fixture",
        "offer_url": "https://app.paseo.sh/#offer=fixture",
        "gateway_pubkey": gateway.public_key().to_hex(),
    })
}

fn answer(fake: &mut Fake, op: Op, payload: Value) -> Value {
    match op {
        Op::Profile => {
            let mut reply = json!({"ok": true, "developer": "ada", "profile": fake.profile, "paseo_enabled": true});
            if fake.daemon {
                reply["paseo"] = pairing_json(&fake.gateway);
            }
            reply
        }
        Op::Enroll if fake.refuse_enroll => json!({
            "ok": false, "code": "github_login_mismatch",
            "error": "GitHub reports another login for the profile's token",
        }),
        Op::Enroll => {
            let mut models: Vec<String> = fake.profile["models"]
                .as_array()
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|m| m.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(sent) = payload["profile"]["models"].as_object() {
                models.extend(sent.keys().cloned());
            }
            fake.profile = json!({"models": models});
            fake.daemon = true;
            fake.enrolls.push(payload);
            json!({"ok": true, "developer": "ada", "version_id": "v", "profile": fake.profile, "paseo": pairing_json(&fake.gateway)})
        }
        other => panic!("unexpected op {other:?}"),
    }
}

async fn manager_socket(fake: Shared, mut socket: WebSocket) {
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
            "AUTH" => socket
                .send(send(json!(["OK", frame[1]["id"], true, ""])))
                .await
                .unwrap(),
            "REQ" => socket.send(send(json!(["EOSE", frame[1]]))).await.unwrap(),
            "EVENT" => {
                let event: Event = serde_json::from_value(frame[1].clone()).unwrap();
                socket
                    .send(send(json!(["OK", event.id, true, ""])))
                    .await
                    .unwrap();
                let reply = {
                    let mut fake = fake.lock().unwrap();
                    let opened = buzz_session_control::open_request(&event, &fake.manager).unwrap();
                    assert!(opened.auth_tag.is_none(), "setup signs as the owner");
                    let response = answer(&mut fake, opened.request.op, opened.request.payload);
                    buzz_session_control::build_reply(
                        &fake.manager,
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
                    upgrade.on_upgrade(move |socket| manager_socket(fake, socket))
                },
            ),
        )
        // No earlier delegation heads: each grant starts at the current time.
        .route("/query", post(|| async { Json(json!([])) }))
        .route(
            "/events",
            post(
                |State(fake): State<Shared>, Json(event): Json<Event>| async move {
                    event.verify().unwrap();
                    let id = event.id;
                    fake.lock().unwrap().grants.push(event);
                    Json(json!({"event_id": id, "accepted": true, "message": ""}))
                },
            ),
        )
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address.to_string()
}

static KEYS_ADDED: AtomicUsize = AtomicUsize::new(0);

fn options(github_api: &str) -> Options {
    Options {
        developer: None,
        local: Local {
            git_email: Some("ada@example.com".into()),
            via_profile: profile::SHARED_VIA_PROFILE.into(),
            via_explicit: false,
        },
        update: false,
        github_api: github_api.into(),
        keygen: |_| {
            Ok(profile::SigningKey {
                private:
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n"
                        .into(),
                public: "ssh-ed25519 AAAA".into(),
            })
        },
        exclude: Vec::new(),
        pending_channel: None,
        replace_signing_key: false,
        add_key: |_, public| {
            assert_eq!(public, "ssh-ed25519 AAAA");
            KEYS_ADDED.fetch_add(1, Ordering::SeqCst);
            true
        },
    }
}

async fn github() -> String {
    let app = Router::new().route(
        "/user",
        get(|| async { Json(json!({"login": "ada-l", "name": "Ada Lovelace"})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

const SECRETS: [&str; 4] = [
    "ghp_fixture",
    "ops_fixture",
    "sk-ant-oat01-fixture",
    "github_pat_fixture",
];

#[tokio::test]
async fn setup_onboards_a_new_developer_then_renews_and_fills_gaps() {
    let owner = Keys::generate();
    let gateway = Keys::generate();
    let fake = Arc::new(Mutex::new(Fake {
        manager: Keys::generate(),
        gateway: gateway.clone(),
        profile: Value::Null,
        daemon: false,
        refuse_enroll: false,
        enrolls: Vec::new(),
        grants: Vec::new(),
    }));
    let address = serve(fake.clone()).await;
    let client = BuzzClient::new(format!("http://{address}"), owner.clone(), None, None).unwrap();
    let manager = Manager {
        relay: format!("ws://{address}"),
        owner: owner.clone(),
        manager: fake.lock().unwrap().manager.public_key(),
    };
    let api = github().await;

    // 1. A new developer: full profile, daemon token, signing key, grant.
    let mut script = Script::new(&[
        ("GitHub token", Some("ghp_fixture")),
        ("Git name", None),
        ("Git email", None),
        ("1Password service account token", Some("ops_fixture")),
        ("Claude subscription token", Some("sk-ant-oat01-fixture")),
        ("Anthropic API key", None),
        ("Codex access token", None),
        ("OpenAI API key", None),
        ("OpenRouter API key", None),
        ("Paseo GitHub read token", Some("github_pat_fixture")),
    ]);
    let outcome = run(&client, &manager, &mut script, options(&api))
        .await
        .unwrap();
    assert!(script.finished());
    assert_eq!(outcome.password.as_str(), URL_SAFE_NO_PAD.encode([9u8; 32]));
    assert_eq!(outcome.report["developer"], "ada");
    assert_eq!(outcome.report["enrolled"], true);
    let report = outcome.report.to_string();
    for secret in SECRETS {
        assert!(!report.contains(secret), "the report leaked {secret}");
    }
    assert!(!report.contains(outcome.password.as_str()));
    {
        let fake = fake.lock().unwrap();
        let enroll = &fake.enrolls[0];
        assert_eq!(enroll["profile"]["developer"], "ada");
        assert_eq!(
            enroll["profile"]["owner_pubkey"],
            owner.public_key().to_hex()
        );
        assert_eq!(enroll["profile"]["git"]["name"], "Ada Lovelace");
        assert_eq!(enroll["profile"]["github"]["login"], "ada-l");
        assert_eq!(enroll["paseo"]["github_read_token"], "github_pat_fixture");
        let (grant, _) = Delegation::parse(&fake.grants[0]).unwrap();
        assert!(grant.enabled && grant.gateway == gateway.public_key());
        assert_eq!(fake.grants[0].pubkey, owner.public_key());
    }
    assert_eq!(KEYS_ADDED.load(Ordering::SeqCst), 1);

    // 2. A rerun asks nothing and enrolls nothing: it renews the grant.
    let mut silent = Script::default();
    let rerun = run(&client, &manager, &mut silent, options(&api))
        .await
        .unwrap();
    assert_eq!(rerun.report["enrolled"], false);
    assert_eq!(fake.lock().unwrap().enrolls.len(), 1);
    assert_eq!(fake.lock().unwrap().grants.len(), 2);

    // 3. A stored profile without a Claude subscription token (an API key
    // does not reach the assistant): setup asks for one, says it replaces
    // the API key, and enrolls only that, keeping everything else stored.
    fake.lock().unwrap().profile = json!({"models": ["anthropic_api_key"]});
    let mut gap = Script::new(&[("Claude subscription token", Some("sk-ant-oat01-fixture"))]);
    run(&client, &manager, &mut gap, options(&api))
        .await
        .unwrap();
    assert!(gap.finished());
    assert!(gap
        .notes
        .iter()
        .any(|note| note.contains("replaces your Anthropic API key")));
    let partial = fake.lock().unwrap().enrolls[1].clone();
    assert_eq!(
        partial,
        json!({"profile": {
            "version": 1, "owner_pubkey": owner.public_key().to_hex(), "developer": "ada",
            "models": {"claude_oauth_token": "sk-ant-oat01-fixture"},
        }})
    );

    // 4. A refused enrollment reports the manager's code and grants nothing.
    {
        let mut fake = fake.lock().unwrap();
        fake.profile = Value::Null;
        fake.daemon = false;
        fake.refuse_enroll = true;
    }
    let grants = fake.lock().unwrap().grants.len();
    let mut refused = Script::new(&[
        ("GitHub token", Some("ghp_fixture")),
        ("Git name", None),
        ("Git email", None),
        ("1Password service account token", Some("ops_fixture")),
        ("Claude subscription token", None),
        ("Anthropic API key", None),
        ("Codex access token", None),
        ("OpenAI API key", None),
        ("OpenRouter API key", None),
        ("Paseo GitHub read token", None),
    ]);
    let error = run(&client, &manager, &mut refused, options(&api))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("github_login_mismatch"), "{error}");
    assert_eq!(fake.lock().unwrap().grants.len(), grants);
    assert_eq!(
        KEYS_ADDED.load(Ordering::SeqCst),
        1,
        "no key added for a refused profile"
    );
}
