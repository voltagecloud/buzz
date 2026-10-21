//! buzz-app's pairing socket tests (`src-tauri/src/pairing/relay_tests.rs`).

use super::relay::*;
use buzz_pairing::PairingSession;
use futures::{SinkExt, StreamExt};
use nostr_pairing::JsonUtil;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use url::Url;

async fn sockets() -> (Socket, WebSocketStream<tokio::net::TcpStream>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap()
    });
    let (client, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    (client, server.await.unwrap())
}
async fn send_server(
    server: &mut WebSocketStream<tokio::net::TcpStream>,
    value: serde_json::Value,
) {
    server
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn read_server(server: &mut WebSocketStream<tokio::net::TcpStream>) -> serde_json::Value {
    let message = server.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
#[tokio::test]
async fn subscription_preserves_an_offer_arriving_before_eose() {
    let (mut client, mut server) = sockets().await;
    let relay = Url::parse("wss://relay.test/pair").unwrap();
    let (source, qr) = PairingSession::new_source(relay.to_string());
    let (_, offer) = PairingSession::new_target(&qr).unwrap();
    send_server(&mut server, serde_json::json!(["EVENT", "pair", offer])).await;
    send_server(&mut server, serde_json::json!(["EOSE", "pair"])).await;
    let (pending, _) = subscribe(&mut client, &source, &relay).await.unwrap();
    assert_eq!(pending, vec![offer]);
    assert_eq!(read_server(&mut server).await[0], "REQ");
}
#[tokio::test]
async fn closed_subscription_never_becomes_ready() {
    let (mut client, mut server) = sockets().await;
    let relay = Url::parse("wss://relay.test/pair").unwrap();
    let (source, _) = PairingSession::new_source(relay.to_string());
    send_server(
        &mut server,
        serde_json::json!(["CLOSED", "pair", "restricted"]),
    )
    .await;
    assert!(subscribe(&mut client, &source, &relay).await.is_err());
}
#[tokio::test]
async fn late_auth_uses_ephemeral_key_and_retries_only_unacknowledged_events() {
    let (mut client, mut server) = sockets().await;
    let relay = Url::parse("wss://relay.test/pair").unwrap();
    let (source, _) = PairingSession::new_source(relay.to_string());
    send_server(&mut server, serde_json::json!(["EOSE", "pair"])).await;
    let (_, mut auth) = subscribe(&mut client, &source, &relay).await.unwrap();
    assert_eq!(read_server(&mut server).await[0], "REQ");
    let event = source
        .sign_event(nostr_pairing::EventBuilder::new(
            nostr_pairing::Kind::Custom(24134),
            "encrypted-fixture",
        ))
        .unwrap();
    auth.unacknowledged.push(event.clone());
    send_server(&mut server, serde_json::json!(["AUTH", "challenge"])).await;
    let message = next(&mut client).await.unwrap();
    assert!(
        auth.handle(&mut client, &source, &relay, &message)
            .await
            .unwrap()
    );
    let authentication = read_server(&mut server).await;
    assert_eq!(authentication[0], "AUTH");
    let proof = nostr_pairing::Event::from_json(authentication[1].to_string()).unwrap();
    assert!(proof.verify().is_ok());
    assert_eq!(proof.pubkey, source.pubkey());
    assert_eq!(proof.kind, nostr_pairing::Kind::Authentication);
    let rejected = serde_json::json!([
        "OK",
        event.id.to_hex(),
        false,
        "auth-required: authenticate"
    ]);
    assert!(
        auth.handle(&mut client, &source, &relay, &rejected)
            .await
            .unwrap()
    );
    let accepted = serde_json::json!(["OK", proof.id.to_hex(), true, ""]);
    assert!(
        auth.handle(&mut client, &source, &relay, &accepted)
            .await
            .unwrap()
    );
    assert_eq!(read_server(&mut server).await[0], "REQ");
    let replay = read_server(&mut server).await;
    assert_eq!(replay[1]["id"], event.id.to_hex());
    auth.handle(
        &mut client,
        &source,
        &relay,
        &serde_json::json!(["OK", event.id.to_hex(), true, ""]),
    )
    .await
    .unwrap();
    assert!(auth.unacknowledged.is_empty());
}

#[tokio::test]
async fn stalled_connection_has_a_retryable_transport_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let relay = Url::parse(&format!("ws://{address}")).unwrap();
    let result = connect(&relay, std::time::Duration::from_millis(50)).await;
    assert!(result.unwrap_err().contains("connection timed out"));
    server.abort();
}
