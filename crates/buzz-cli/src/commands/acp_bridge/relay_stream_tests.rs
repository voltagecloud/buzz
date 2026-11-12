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
