use super::*;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use nostr::{EventBuilder, Keys, Kind, Tag};
use std::sync::{Arc, RwLock};

type Stored = Arc<RwLock<Vec<Event>>>;

#[tokio::test]
async fn relay_signing_identity_uses_nip11_self_instead_of_optional_operator_pubkey() {
    let signer = Keys::generate().public_key();
    let operator = Keys::generate().public_key();
    for document in [
        json!({"self":signer,"pubkey":null}),
        json!({"self":signer,"pubkey":operator}),
        json!({"pubkey":operator}),
    ] {
        let expected = document.get("self").is_some();
        let app = Router::new().route(
            "/info",
            get(move || {
                let document = document.clone();
                async move { Json(document) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            BuzzClient::new(format!("http://{address}"), Keys::generate(), None, None).unwrap();
        let result = relay_key(&client).await;
        if expected {
            assert_eq!(result.unwrap(), signer);
        } else {
            assert!(result.unwrap_err().to_string().contains("self signing key"));
        }
        server.abort();
    }
}
async fn serve(State(stored): State<Stored>, Json(filters): Json<Vec<Value>>) -> Json<Vec<Event>> {
    let filter = &filters[0];
    assert_eq!(
        filter["consistency"], "strong",
        "production authority reads must route to the writer"
    );
    let events = stored.read().unwrap();
    Json(
        events
            .iter()
            .filter(|event| {
                filter["kinds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|kind| kind.as_u64() == Some(event.kind.as_u16() as u64))
                    && filter.get("authors").is_none_or(|authors| {
                        authors
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|author| author.as_str() == Some(event.pubkey.to_hex().as_str()))
                    })
                    && filter.get("#d").is_none_or(|values| {
                        values
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|value| value.as_str() == tag(event, "d"))
                    })
            })
            .cloned()
            .collect(),
    )
}
async fn fixture(events: Vec<Event>) -> (BuzzClient, Stored, tokio::task::JoinHandle<()>) {
    let stored = Arc::new(RwLock::new(events));
    let app = Router::new()
        .route("/query", post(serve))
        .with_state(stored.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        BuzzClient::new(format!("http://{address}"), Keys::generate(), None, None).unwrap(),
        stored,
        server,
    )
}
#[tokio::test]
async fn current_authority_refresh_uses_strong_reads_and_detects_withdrawal_removal_and_revocation()
{
    let owner = Keys::generate();
    let agent = Keys::generate();
    let relay = Keys::generate();
    let scope = Scope {
        channel: Uuid::new_v4(),
        thread: EventId::all_zeros(),
        agent: agent.public_key(),
    };
    let request = wire::request(&owner, scope, Uuid::new_v4(), now() + 600).unwrap();
    let pending = Pending::parse(&request, now()).unwrap();
    let approval = wire::approve(&owner, &pending, now() + 599).unwrap();
    let grant = Grant::parse(&approval, scope, now()).unwrap();
    let roster = EventBuilder::new(Kind::from(39002), "")
        .tags([
            Tag::parse(["d", &scope.channel.to_string()]).unwrap(),
            Tag::parse(["p", &owner.public_key().to_hex(), "", "member"]).unwrap(),
            Tag::parse(["p", &agent.public_key().to_hex(), "", "member"]).unwrap(),
        ])
        .sign_with_keys(&relay)
        .unwrap();
    let (client, stored, server) =
        fixture(vec![request.clone(), approval.clone(), roster.clone()]).await;
    authorized(
        &client,
        owner.public_key(),
        relay.public_key(),
        &pending,
        &grant,
    )
    .await
    .unwrap();
    stored
        .write()
        .unwrap()
        .retain(|event| event.id != request.id);
    assert!(
        authorized(
            &client,
            owner.public_key(),
            relay.public_key(),
            &pending,
            &grant
        )
        .await
        .is_err(),
        "request withdrawal must end the terminal"
    );
    *stored.write().unwrap() = vec![request.clone(), roster.clone()];
    assert!(
        authorized(
            &client,
            owner.public_key(),
            relay.public_key(),
            &pending,
            &grant
        )
        .await
        .is_err(),
        "deleted approval must end the terminal"
    );
    let removed = EventBuilder::new(Kind::from(39002), "")
        .tag(Tag::parse(["d", &scope.channel.to_string()]).unwrap())
        .sign_with_keys(&relay)
        .unwrap();
    *stored.write().unwrap() = vec![request, approval, removed];
    assert!(
        authorized(
            &client,
            owner.public_key(),
            relay.public_key(),
            &pending,
            &grant
        )
        .await
        .is_err(),
        "removal must end the terminal"
    );
    *stored.write().unwrap() = vec![];
    assert!(
        authorized(
            &client,
            owner.public_key(),
            relay.public_key(),
            &pending,
            &grant
        )
        .await
        .is_err(),
        "missing authority must fail closed"
    );
    server.abort();
}
#[tokio::test]
async fn plaintext_relays_are_rejected_by_real_dispatch_before_network() {
    let client =
        BuzzClient::new("http://127.0.0.1:9".into(), Keys::generate(), None, None).unwrap();
    let error = dispatch(
        SandboxCmd::Authorize {
            request: EventId::all_zeros(),
            minutes: 15,
        },
        &client,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("HTTPS/WSS"));
    for relay in [
        "ws://localhost",
        "https://user:secret@example.com",
        "https://example.com/?token=x",
        "https://example.com/#x",
    ] {
        assert!(websocket_url(relay).is_err());
    }
    assert_eq!(
        websocket_url("https://example.com").unwrap(),
        "wss://example.com/"
    );
}
#[test]
fn configured_targets_and_untrusted_labels_have_budgets() {
    let agent = Keys::generate().public_key();
    let channel = Uuid::new_v4();
    let entry = json!({"id":"agent","label":"Agent","agent":agent,"channel":channel});
    assert_eq!(
        mappings(&json!([entry.clone()]).to_string()).unwrap().len(),
        1
    );
    assert!(mappings(&Value::Array(vec![entry; 65]).to_string()).is_err());
    assert!(mappings(&" ".repeat(65537)).is_err());
    assert_eq!(label("bad\x1b[2J\n\u{202e}name"), "bad[2Jname");
    assert_eq!(label(&"x".repeat(1000)).len(), 100);
}

/// Ordinary cargo/CI runs create a controlling PTY for this production flow.
#[cfg(unix)]
#[tokio::test]
async fn real_tty_forwards_encrypted_raw_bytes_and_restores_after_close() {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use buzz_sdk::sandbox_attach::{Flow, Packet};
    if std::env::var_os("BUZZ_SANDBOX_TEST_PTY_CHILD").is_none() {
        let driver = r#"
import fcntl, os, pty, select, struct, subprocess, sys, termios, time
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
original = termios.tcgetattr(slave)
def child():
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
process = subprocess.Popen([sys.argv[1], sys.argv[2], '--exact', '--nocapture'],
    stdin=slave, stdout=slave, stderr=slave, preexec_fn=child,
    env=dict(os.environ, BUZZ_SANDBOX_TEST_PTY_CHILD='1'))
output = bytearray()
sent = False
deadline = time.monotonic() + 25
def consume():
    global sent
    output.extend(os.read(master, 65536))
    if len(output) > 1048576:
        raise RuntimeError('PTY test exceeded its output budget')
    if b'REMOTE_OUTPUT_OK' in output and not sent:
        os.write(master, b'\x03\x1b[Araw\r\n')
        sent = True
try:
    while process.poll() is None and time.monotonic() < deadline:
        if select.select([master], [], [], .1)[0]:
            consume()
    process.wait(timeout=2)
    while select.select([master], [], [], 0)[0]:
        consume()
    assert process.returncode == 0 and sent and b'REAL_PTY_SUCCESS' in output, output.decode(errors='replace')
    assert original == termios.tcgetattr(slave), 'terminal attributes not restored'
    print('Controlling PTY: encrypted control bytes and complete termios restoration passed.')
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
    os.close(slave)
"#;
        let executable = std::env::current_exe().unwrap();
        let name = "commands::sandbox::tests::real_tty_forwards_encrypted_raw_bytes_and_restores_after_close";
        let result = std::process::Command::new("python3")
            .args(["-c", driver])
            .arg(executable)
            .arg(name)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "PTY driver failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    assert!(
        std::io::stdin().is_terminal(),
        "PTY child must own a terminal"
    );
    async fn receive(socket: &mut WebSocket) -> Value {
        let frame = timeout(Duration::from_secs(10), socket.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Text(text) = frame else {
            panic!("expected signed relay frame")
        };
        serde_json::from_str(text.as_str()).unwrap()
    }
    let owner = Keys::generate();
    let agent = Keys::generate();
    let relay = Keys::generate();
    let scope = Scope {
        channel: Uuid::new_v4(),
        thread: EventId::all_zeros(),
        agent: agent.public_key(),
    };
    let request = wire::request(&owner, scope, Uuid::new_v4(), now() + 600).unwrap();
    let pending = Pending::parse(&request, now()).unwrap();
    let approval = wire::approve(&owner, &pending, now() + 599).unwrap();
    let grant = Grant::parse(&approval, scope, now()).unwrap();
    let roster = EventBuilder::new(Kind::from(39002), "")
        .tags([
            Tag::parse(["d", &scope.channel.to_string()]).unwrap(),
            Tag::parse(["p", &owner.public_key().to_hex(), "", "member"]).unwrap(),
            Tag::parse(["p", &agent.public_key().to_hex(), "", "member"]).unwrap(),
        ])
        .sign_with_keys(&relay)
        .unwrap();
    let (http, _, authority_server) = fixture(vec![request, approval, roster]).await;
    let client = BuzzClient::new(http.relay_url().to_owned(), owner.clone(), None, None).unwrap();
    let grant_id = grant.id();
    let requester = owner.public_key();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let done = Arc::new(std::sync::Mutex::new(Some(done_tx)));
    let router = Router::new().route(
        "/",
        get(move |upgrade: WebSocketUpgrade| {
            let agent = agent.clone();
            let done = done.clone();
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    socket
                        .send(Message::Text(
                            json!(["AUTH", "test-challenge"]).to_string().into(),
                        ))
                        .await
                        .unwrap();
                    let auth = receive(&mut socket).await;
                    assert_eq!(auth[0], "AUTH");
                    let event: Event = serde_json::from_value(auth[1].clone()).unwrap();
                    event.verify().unwrap();
                    assert_eq!(event.pubkey, requester);
                    socket
                        .send(Message::Text(
                            json!(["OK", event.id, true, ""]).to_string().into(),
                        ))
                        .await
                        .unwrap();
                    assert_eq!(receive(&mut socket).await[0], "REQ");
                    let event = receive(&mut socket).await;
                    let event: Event = serde_json::from_value(event[1].clone()).unwrap();
                    let mut flow = Flow::new(grant_id);
                    let open =
                        wire::decrypt(&agent, requester, scope, grant_id, &event, now()).unwrap();
                    assert!(matches!(
                        flow.receive(open).unwrap(),
                        Some(Packet::Open { .. })
                    ));
                    let frame = flow
                        .send(Packet::data(b"REMOTE_OUTPUT_OK\r\n").unwrap(), 0)
                        .unwrap();
                    let event = wire::encrypt(&agent, requester, scope, &frame).unwrap();
                    socket
                        .send(Message::Text(
                            json!(["EVENT", "sandbox-terminal", event])
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    loop {
                        let value = receive(&mut socket).await;
                        let event: Event = serde_json::from_value(value[1].clone()).unwrap();
                        let frame =
                            wire::decrypt(&agent, requester, scope, grant_id, &event, now())
                                .unwrap();
                        if let Some(packet) = flow.receive(frame).unwrap() {
                            assert_eq!(packet.bytes().unwrap(), b"\x03\x1b[Araw\r\n");
                            break;
                        }
                    }
                    let close = flow.send(Packet::Close, 0).unwrap();
                    let event = wire::encrypt(&agent, requester, scope, &close).unwrap();
                    socket
                        .send(Message::Text(
                            json!(["EVENT", "sandbox-terminal", event])
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    done.lock().unwrap().take().unwrap().send(()).unwrap();
                    sleep(Duration::from_millis(300)).await;
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let websocket_server =
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    timeout(
        Duration::from_secs(15),
        terminal::attach(
            &client,
            &url,
            owner.public_key(),
            relay.public_key(),
            &pending,
            &grant,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    done_rx.await.unwrap();
    assert!(
        !crossterm::terminal::is_raw_mode_enabled().unwrap(),
        "raw mode must be restored after peer close"
    );
    println!("REAL_PTY_SUCCESS");
    authority_server.abort();
    websocket_server.abort();
}
