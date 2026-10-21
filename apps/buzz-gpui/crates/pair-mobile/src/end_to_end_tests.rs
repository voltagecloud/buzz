//! Whole attempts through [`Pairing::start`], with a phone simulated by
//! buzz-pairing on the other end of a local pairing relay.
//!
//! The shared test relay can't carry these: it doesn't run the pairing
//! sidecar its NIP-11 document points to (`/pair`), and its main socket
//! admits members only. The relay here behaves like the sidecar, plus a
//! NIP-42 challenge on connect, which Buzz relays send. Discovery maps a
//! made-up community origin to it, since the sidecar's address must be
//! `wss://`.

use super::*;
use buzz_pairing::{PairingMessage, crypto, qr::QrPayload};
use futures::{SinkExt, StreamExt};
use nostr_pairing::{EventBuilder, JsonUtil, Keys, Kind, PublicKey, RelayUrl, Tag};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

static SHOWN: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
static RELAYS: LazyLock<Mutex<HashMap<String, url::Url>>> = LazyLock::new(Default::default);

/// Records the URI behind attempt `id`'s QR code, which the phone scans.
pub(super) fn shown(id: &str, uri: &str) {
    SHOWN.lock().unwrap().insert(id.into(), uri.into());
}

/// The local pairing relay standing in for `origin`'s sidecar.
pub(super) fn pairing_relay(origin: &url::Url) -> Option<url::Url> {
    RELAYS.lock().unwrap().get(origin.as_str()).cloned()
}

/// A sidecar-like relay: kind 24134 only, `#p` subscriptions, history for
/// offers that arrive before the desktop subscribes, and a NIP-42
/// challenge that must be answered before a subscription is accepted.
async fn pairing_relay_for(community: &str) -> url::Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (events, _) = broadcast::channel::<Value>(64);
    let history = Arc::new(Mutex::new(Vec::<Value>::new()));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let events = events.clone();
            let history = history.clone();
            tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut feed = events.subscribe();
                let mut authed = false;
                let mut subscriptions: Vec<(String, String)> = Vec::new();
                let send = |value: Value| Message::Text(value.to_string().into());
                socket
                    .send(send(json!(["AUTH", "pairing-challenge"])))
                    .await
                    .unwrap();
                loop {
                    tokio::select! {
                        Ok(event) = feed.recv() => {
                            for (sub, p) in &subscriptions {
                                if tagged(&event, p) {
                                    let _ = socket.send(send(json!(["EVENT", sub, event]))).await;
                                }
                            }
                        }
                        message = socket.next() => {
                            let Some(Ok(Message::Text(text))) = message else { break };
                            let value: Value = serde_json::from_str(&text).unwrap();
                            match value[0].as_str() {
                                Some("AUTH") => {
                                    let auth = nostr_pairing::Event::from_json(value[1].to_string()).unwrap();
                                    authed = auth.verify().is_ok() && auth.kind == Kind::Authentication;
                                    let _ = socket.send(send(json!(["OK", auth.id.to_hex(), authed, ""]))).await;
                                }
                                Some("REQ") if !authed => {
                                    let _ = socket.send(send(json!(["CLOSED", value[1], "auth-required: answer the challenge"]))).await;
                                }
                                Some("REQ") => {
                                    let sub = value[1].as_str().unwrap().to_string();
                                    let p = value[2]["#p"][0].as_str().unwrap().to_string();
                                    let past = history.lock().unwrap().clone();
                                    for event in past.iter().filter(|e| tagged(e, &p)) {
                                        let _ = socket.send(send(json!(["EVENT", sub, event]))).await;
                                    }
                                    let _ = socket.send(send(json!(["EOSE", sub]))).await;
                                    subscriptions.push((sub, p));
                                }
                                Some("EVENT") => {
                                    let event = nostr_pairing::Event::from_json(value[1].to_string()).unwrap();
                                    let ok = event.verify().is_ok() && event.kind == Kind::Custom(24134);
                                    let _ = socket.send(send(json!(["OK", event.id.to_hex(), ok, ""]))).await;
                                    if ok {
                                        history.lock().unwrap().push(value[1].clone());
                                        let _ = events.send(value[1].clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
        }
    });
    let relay = url::Url::parse(&format!("ws://{address}/pair")).unwrap();
    RELAYS.lock().unwrap().insert(
        relay::community(community).unwrap().to_string(),
        relay.clone(),
    );
    relay
}

fn tagged(event: &Value, pubkey: &str) -> bool {
    event["tags"]
        .as_array()
        .is_some_and(|tags| tags.iter().any(|t| t[0] == "p" && t[1] == pubkey))
}

/// The phone's own connection to the pairing relay, subscribed to `me`.
struct Phone {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Phone {
    async fn connect(relay: &url::Url, me: PublicKey) -> Self {
        let (mut socket, _) = tokio_tungstenite::connect_async(relay.as_str())
            .await
            .unwrap();
        let challenge = Self::read(&mut socket).await;
        assert_eq!(challenge[0], "AUTH");
        let auth = EventBuilder::auth(
            challenge[1].as_str().unwrap(),
            RelayUrl::parse(relay.as_str()).unwrap(),
        )
        .sign_with_keys(&Keys::generate())
        .unwrap();
        socket
            .send(Message::Text(json!(["AUTH", auth]).to_string().into()))
            .await
            .unwrap();
        assert_eq!(Self::read(&mut socket).await[2], true);
        let request = json!(["REQ", "phone", {"kinds": [24134], "#p": [me.to_hex()]}]);
        socket
            .send(Message::Text(request.to_string().into()))
            .await
            .unwrap();
        assert_eq!(Self::read(&mut socket).await[0], "EOSE");
        Self { socket }
    }

    async fn read(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("the relay answers")
            .unwrap()
            .unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }

    async fn publish(&mut self, event: &Event) {
        self.socket
            .send(Message::Text(json!(["EVENT", event]).to_string().into()))
            .await
            .unwrap();
    }

    /// The next pairing event addressed to the phone.
    async fn receive(&mut self) -> Event {
        loop {
            let message = Self::read(&mut self.socket).await;
            if message[0] == "EVENT" {
                return Event::from_json(message[2].to_string()).unwrap();
            }
        }
    }
}

/// An encrypted message from a phone that speaks with its own keys, as
/// buzz-app's tests build them (the session type keeps its keys private).
fn from_phone(keys: &Keys, desktop: &PublicKey, value: PairingMessage) -> Event {
    let content = nostr_pairing::nips::nip44::encrypt(
        keys.secret_key(),
        desktop,
        serde_json::to_string(&value).unwrap(),
        nostr_pairing::nips::nip44::Version::V2,
    )
    .unwrap();
    EventBuilder::new(Kind::Custom(buzz_pairing::session::KIND_PAIRING), content)
        .tags([Tag::public_key(*desktop)])
        .sign_with_keys(keys)
        .unwrap()
}

fn open(keys: &Keys, event: &Event) -> PairingMessage {
    let plain =
        nostr_pairing::nips::nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)
            .unwrap();
    serde_json::from_str(&plain).unwrap()
}

/// Polls like the card does until `done` accepts the status.
async fn wait(pairing: &Pairing, id: &str, done: impl Fn(&Status) -> bool) -> Status {
    for _ in 0..200 {
        let status = pairing.status(id).unwrap();
        if done(&status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("stuck at {:?}", pairing.status(id));
}

/// Starts an attempt for `identity` against `community` and scans its QR.
async fn begin(
    community: &str,
    id: &str,
    identity: &Arc<Identity>,
) -> (Pairing, url::Url, QrPayload) {
    let relay = pairing_relay_for(community).await;
    let pairing = Pairing::default();
    pairing
        .start(
            &tokio::runtime::Handle::current(),
            Some(identity.clone()),
            id,
            &identity.pubkey_hex(),
            community,
        )
        .unwrap();
    let Status::Qr {
        matrix,
        uri: copied,
    } = wait(&pairing, id, |s| !matches!(s, Status::Connecting)).await
    else {
        panic!("no QR: {:?}", pairing.status(id));
    };
    let uri = SHOWN.lock().unwrap().get(id).cloned().unwrap();
    // "Copy pairing code" copies exactly what the QR shows.
    assert_eq!(copied, uri);
    assert_eq!(
        *matrix,
        QrMatrix::encode(&uri).unwrap(),
        "the QR shows the URI"
    );
    let qr = buzz_pairing::qr::decode_qr(&uri).unwrap();
    assert_eq!(qr.relays, vec![relay.to_string()]);
    (pairing, relay, qr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phone_enters_the_desktop_code_and_receives_the_account() {
    let identity = Arc::new(Identity::ephemeral());
    let id = "00000000-0000-4000-8000-000000000001";
    let (pairing, relay, qr) = begin("wss://code-entry.test", id, &identity).await;
    let phone_keys = Keys::generate();
    let mut phone = Phone::connect(&relay, phone_keys.public_key()).await;
    phone
        .publish(&from_phone(
            &phone_keys,
            &qr.source_pubkey,
            PairingMessage::Offer {
                session_id: hex::encode(crypto::derive_session_id(&qr.session_secret)),
                version: 1,
                confirmation: Some("desktop-code-v1".into()),
            },
        ))
        .await;
    let Status::Code { code, code_entry } =
        wait(&pairing, id, |s| matches!(s, Status::Code { .. })).await
    else {
        unreachable!()
    };
    assert!(code_entry);
    assert_eq!(code.len(), 6);
    // The challenge carries no code.
    assert_eq!(
        open(&phone_keys, &phone.receive().await),
        PairingMessage::DesktopCode {}
    );
    let wrong = if code == "000000" { "000001" } else { "000000" };
    phone
        .publish(&from_phone(
            &phone_keys,
            &qr.source_pubkey,
            PairingMessage::CodeSubmit {
                code: wrong.into(),
                request_id: "1".into(),
            },
        ))
        .await;
    assert!(matches!(
        open(&phone_keys, &phone.receive().await),
        PairingMessage::CodeRejected {
            remaining_attempts: 4,
            ..
        }
    ));
    assert!(matches!(pairing.status(id).unwrap(), Status::Code { .. }));
    phone
        .publish(&from_phone(
            &phone_keys,
            &qr.source_pubkey,
            PairingMessage::CodeSubmit {
                code,
                request_id: "2".into(),
            },
        ))
        .await;
    assert!(matches!(
        open(&phone_keys, &phone.receive().await),
        PairingMessage::SasConfirm { .. }
    ));
    let PairingMessage::Payload { payload, .. } = open(&phone_keys, &phone.receive().await) else {
        panic!("no account")
    };
    let account: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(account["relayUrl"], "https://code-entry.test");
    assert_eq!(account["pubkey"], identity.pubkey_hex());
    assert_eq!(account["nsec"].as_str().unwrap(), &*identity.nsec());
    assert_eq!(
        wait(&pairing, id, |s| !matches!(s, Status::Code { .. })).await,
        Status::Transferring
    );
    phone
        .publish(&from_phone(
            &phone_keys,
            &qr.source_pubkey,
            PairingMessage::Complete { success: true },
        ))
        .await;
    assert_eq!(
        wait(&pairing, id, |s| *s != Status::Transferring).await,
        Status::Complete
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_phone_waits_for_codes_match() {
    let identity = Arc::new(Identity::ephemeral());
    let id = "00000000-0000-4000-8000-000000000002";
    let (pairing, relay, qr) = begin("wss://legacy.test", id, &identity).await;
    let (mut target, offer) = PairingSession::new_target(&qr).unwrap();
    let mut phone = Phone::connect(&relay, target.pubkey()).await;
    phone.publish(&offer).await;
    let status = wait(&pairing, id, |s| matches!(s, Status::Code { .. })).await;
    assert_eq!(
        status,
        Status::Code {
            code: target.sas_code().unwrap(),
            code_entry: false
        },
        "both devices show the same digits"
    );
    pairing.confirm(id).unwrap();
    assert_eq!(pairing.status(id).unwrap(), Status::Transferring);
    target.handle_sas_confirm(&phone.receive().await).unwrap();
    target.confirm_target_sas().unwrap();
    let (_, payload) = target.handle_payload(&phone.receive().await).unwrap();
    let account: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(account["pubkey"], identity.pubkey_hex());
    phone.publish(&target.send_complete().unwrap()).await;
    assert_eq!(
        wait(&pairing, id, |s| *s != Status::Transferring).await,
        Status::Complete
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denying_or_cancelling_tells_the_phone_and_keeps_the_account() {
    let identity = Arc::new(Identity::ephemeral());
    let id = "00000000-0000-4000-8000-000000000003";
    let (pairing, relay, qr) = begin("wss://deny.test", id, &identity).await;
    let (mut target, offer) = PairingSession::new_target(&qr).unwrap();
    let mut phone = Phone::connect(&relay, target.pubkey()).await;
    phone.publish(&offer).await;
    wait(&pairing, id, |s| matches!(s, Status::Code { .. })).await;
    pairing.deny(id).unwrap();
    assert_eq!(
        target.handle_abort(&phone.receive().await).unwrap(),
        AbortReason::UserDenied
    );
    assert_eq!(
        wait(&pairing, id, |s| !matches!(s, Status::Code { .. })).await,
        Status::Error {
            message: "Pairing was canceled.".into()
        }
    );

    // Cancelling a fresh attempt before the transfer also notifies.
    let id = "00000000-0000-4000-8000-000000000004";
    let (pairing, relay, qr) = begin("wss://cancel.test", id, &identity).await;
    let (mut target, offer) = PairingSession::new_target(&qr).unwrap();
    let mut phone = Phone::connect(&relay, target.pubkey()).await;
    phone.publish(&offer).await;
    wait(&pairing, id, |s| matches!(s, Status::Code { .. })).await;
    assert_eq!(pairing.cancel(id).await.unwrap(), Status::Cancelled);
    assert_eq!(
        target.handle_abort(&phone.receive().await).unwrap(),
        AbortReason::UserDenied
    );
    assert_eq!(pairing.status(id).unwrap(), Status::Cancelled);
}

#[tokio::test]
async fn unreachable_community_is_a_retryable_error() {
    let identity = Arc::new(Identity::ephemeral());
    let id = "00000000-0000-4000-8000-000000000005";
    let pairing = Pairing::default();
    // Nothing listens on port 9 of this machine.
    RELAYS.lock().unwrap().insert(
        "https://unreachable.test/".into(),
        url::Url::parse("ws://127.0.0.1:9/pair").unwrap(),
    );
    pairing
        .start(
            &tokio::runtime::Handle::current(),
            Some(identity.clone()),
            id,
            &identity.pubkey_hex(),
            "https://unreachable.test",
        )
        .unwrap();
    assert_eq!(
        wait(&pairing, id, |s| *s != Status::Connecting).await,
        Status::Error {
            message: "Couldn’t connect for pairing. Check your connection and try again.".into()
        }
    );
    assert_eq!(
        pairing
            .start(
                &tokio::runtime::Handle::current(),
                Some(identity.clone()),
                "not-a-uuid",
                &identity.pubkey_hex(),
                "https://unreachable.test",
            )
            .unwrap_err(),
        "Unsupported pairing request."
    );
    assert!(
        pairing
            .start(
                &tokio::runtime::Handle::current(),
                Some(identity.clone()),
                id,
                &identity.pubkey_hex(),
                "http://127.0.0.1:53000",
            )
            .is_err()
    );
}
