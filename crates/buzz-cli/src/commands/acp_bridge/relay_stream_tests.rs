//! One relay connection through the real WebSocket client: every stage is
//! bounded in time, and only events with a valid signature reach the bridge.

use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use axum::Router;
use nostr::{EventBuilder, Kind};

#[derive(Clone, Copy, PartialEq)]
enum Relay {
    /// Authenticates, then sends pings but never catches up.
    Stalled,
    /// Authenticates, sends a forged and a genuine event, then catches up.
    Forged,
}

fn scope() -> Scope {
    let signer = Keys::generate();
    Scope {
        owner: signer.public_key(),
        signer,
        agent: Keys::generate().public_key(),
        channel: Uuid::new_v4(),
        home: std::path::PathBuf::from("/nonexistent"),
        paseo_code: std::path::PathBuf::from("/nonexistent/paseo-code"),
    }
}

fn agent_message(agent: &Keys, channel: Uuid, text: &str) -> Event {
    EventBuilder::new(Kind::Custom(9), text)
        .tags([Tag::parse(["h", &channel.to_string()]).unwrap()])
        .sign_with_keys(agent)
        .unwrap()
}

async fn text(socket: &mut WebSocket) -> serde_json::Value {
    loop {
        match socket.recv().await.unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(_) => panic!("client closed"),
            _ => {}
        }
    }
}

async fn send(socket: &mut WebSocket, frame: serde_json::Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn relay(mode: Relay, mut socket: WebSocket, events: Vec<Event>) {
    send(&mut socket, json!(["AUTH", "challenge"])).await;
    let auth = text(&mut socket).await;
    send(&mut socket, json!(["OK", auth[1]["id"], true, ""])).await;
    let (first, second) = (text(&mut socket).await, text(&mut socket).await);
    if mode == Relay::Stalled {
        loop {
            let _ = socket.send(Message::Ping(Vec::new().into())).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    for event in events {
        send(&mut socket, json!(["EVENT", THREAD_SUB, event])).await;
    }
    send(&mut socket, json!(["EOSE", first[1]])).await;
    send(&mut socket, json!(["EOSE", second[1]])).await;
    // Hold the connection open until the client leaves.
    while let Some(Ok(_)) = socket.recv().await {}
}

async fn serve(mode: Relay, events: Vec<Event>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        get(move |upgrade: WebSocketUpgrade| async move {
            upgrade.on_upgrade(move |socket| relay(mode, socket, events))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

const SHORT: Deadlines = Deadlines {
    connect: Duration::from_secs(2),
    catch_up: Duration::from_secs(3),
};

/// Fails the test, instead of hanging it, if a deadline is missing.
async fn bounded(stream: impl Future<Output = Stream>) -> Stream {
    tokio::time::timeout(Duration::from_secs(20), stream)
        .await
        .expect("the connection attempt has no deadline")
}

fn error(stream: Stream) -> String {
    match stream {
        Stream::Failed(failure) => failure.error,
        Stream::ReceiverGone => panic!("expected a failed connection"),
    }
}

#[tokio::test]
async fn each_connection_stage_has_a_deadline() {
    let (tx, _rx) = mpsc::channel(8);

    // TCP accepted, then nothing: the upgrade must not wait forever.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let held = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(socket);
    });
    let silent = error(bounded(stream_once(&url, &scope(), None, 0, &tx, SHORT)).await);
    assert!(silent.contains("did not complete within 2s"), "{silent}");
    held.abort();

    // Authenticated and pinged, but never caught up: pings must not keep the
    // bridge from reconnecting and reporting the relay.
    let url = serve(Relay::Stalled, Vec::new()).await;
    let stalled = error(bounded(stream_once(&url, &scope(), None, 0, &tx, SHORT)).await);
    assert!(stalled.contains("did not catch up within 3s"), "{stalled}");
}

#[tokio::test]
async fn only_verified_relay_events_reach_the_bridge() {
    let scope = scope();
    let agent = Keys::generate();
    let mut forged = agent_message(&agent, scope.channel, "genuine text");
    forged.content = "the session has ended".into();
    let genuine = agent_message(&agent, scope.channel, "genuine reply");
    let url = serve(Relay::Forged, vec![forged, genuine.clone()]).await;
    let (tx, mut rx) = mpsc::channel(8);
    let stream = tokio::spawn(async move { stream_once(&url, &scope, None, 0, &tx, SHORT).await });
    let mut received = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RelayInbound::Event(event) => received.push(event.id),
            RelayInbound::Live => break,
            _ => {}
        }
    }
    assert_eq!(received, vec![genuine.id], "a forged event was forwarded");
    stream.abort();
}

/// Channel changes during reconnect replay must retain older undelivered rows.
#[tokio::test]
async fn replacing_channels_before_catch_up_keeps_the_reconnect_watermark() {
    let scope = scope();
    let initial_channel = scope.channel;
    let (channels_tx, channels_rx) =
        tokio::sync::watch::channel(BTreeMap::from([(initial_channel, now_secs() - 120)]));
    let (requests_tx, mut requests_rx) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        get(move |upgrade: WebSocketUpgrade| {
            let requests = requests_tx.clone();
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    send(&mut socket, json!(["AUTH", "challenge"])).await;
                    let auth = text(&mut socket).await;
                    send(&mut socket, json!(["OK", auth[1]["id"], true, ""])).await;
                    loop {
                        let request = text(&mut socket).await;
                        if request[0] == "REQ"
                            && request[1]
                                .as_str()
                                .is_some_and(|id| id.starts_with(THREAD_SUB))
                        {
                            requests.send(request[2].clone()).unwrap();
                        }
                        // Deliberately withhold EOSE, as a relay still replaying history.
                    }
                })
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let since = now_secs() - 600;
    let (tx, _rx) = mpsc::channel(8);
    let stream = tokio::spawn(async move {
        stream_channels(&url, &scope, None, since, &tx, SHORT, channels_rx).await
    });
    let first = tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["since"], since);
    channels_tx
        .send(BTreeMap::from([
            (initial_channel, since),
            (Uuid::new_v4(), since),
        ]))
        .unwrap();
    let replacement = tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement["since"], since);
    assert_eq!(replacement["#h"].as_array().unwrap().len(), 1);
    stream.abort();
    server.abort();
}

#[tokio::test]
async fn retired_subscription_eose_cannot_finish_a_new_generation() {
    let scope = scope();
    let a = scope.channel;
    let b = Uuid::new_v4();
    let since = now_secs() - 600;
    let (channels_tx, channels_rx) = tokio::sync::watch::channel(BTreeMap::from([(a, since)]));
    let (requests_tx, mut requests_rx) = mpsc::unbounded_channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let finish = Arc::new(tokio::sync::Mutex::new(Some(finish_rx)));
    let marker = agent_message(&Keys::generate(), a, "old EOSE processed");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        get(move |upgrade: WebSocketUpgrade| {
            let (requests, finish, marker) = (requests_tx.clone(), finish.clone(), marker.clone());
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    send(&mut socket, json!(["AUTH", "challenge"])).await;
                    let auth = text(&mut socket).await;
                    send(&mut socket, json!(["OK", auth[1]["id"], true, ""])).await;
                    let telemetry = text(&mut socket).await;
                    let original = text(&mut socket).await;
                    send(&mut socket, json!(["EOSE", telemetry[1]])).await;
                    send(&mut socket, json!(["EOSE", original[1]])).await;
                    let old = text(&mut socket).await;
                    requests.send(old.clone()).unwrap();
                    let closed = text(&mut socket).await;
                    requests.send(closed).unwrap();
                    let new = text(&mut socket).await;
                    requests.send(new.clone()).unwrap();
                    send(&mut socket, json!(["EOSE", old[1]])).await;
                    send(&mut socket, json!(["EVENT", original[1], marker])).await;
                    finish.lock().await.take().unwrap().await.unwrap();
                    send(&mut socket, json!(["EOSE", new[1]])).await;
                    while let Some(Ok(_)) = socket.recv().await {}
                })
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(16);
    let stream = tokio::spawn(async move {
        stream_channels(&url, &scope, None, since, &tx, SHORT, channels_rx).await
    });
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(RelayInbound::Live)
    ));
    channels_tx
        .send(BTreeMap::from([(a, since), (b, since)]))
        .unwrap();
    let old = tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
        .await
        .unwrap()
        .unwrap();
    channels_tx.send(BTreeMap::from([(a, since)])).unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(close, json!(["CLOSE", old[1]]));
    channels_tx
        .send(BTreeMap::from([(a, since), (b, since)]))
        .unwrap();
    let new = tokio::time::timeout(Duration::from_secs(5), requests_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(new[1], old[1]);
    // Drain the legitimate Live caused by removing the only pending channel.
    // The new Down fences it; afterward old EOSE must not produce Live.
    loop {
        if matches!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap(),
            Some(RelayInbound::Down)
        ) {
            break;
        }
    }
    // There are two Down messages (old and new additions); discard the first.
    let mut saw_new_down = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RelayInbound::Down => saw_new_down = true,
            RelayInbound::Live => {
                assert!(!saw_new_down, "stale EOSE completed the new subscription")
            }
            RelayInbound::Event(_) => {
                assert!(saw_new_down);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    finish_tx.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(RelayInbound::Live)
    ));
    stream.abort();
    server.abort();
}
