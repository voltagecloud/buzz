//! buzz-app's native pairing tests (`src-tauri/src/pairing/tests.rs`), against
//! this port of the attempt owner.

use super::*;
use buzz_pairing::{PairingMessage, crypto};
use nostr_pairing::{EventBuilder, JsonUtil, Keys, Kind, Tag};

fn message(keys: &Keys, source: &PairingSession, value: PairingMessage) -> Event {
    let content = nostr_pairing::nips::nip44::encrypt(
        keys.secret_key(),
        &source.pubkey(),
        serde_json::to_string(&value).unwrap(),
        nostr_pairing::nips::nip44::Version::V2,
    )
    .unwrap();
    EventBuilder::new(Kind::Custom(buzz_pairing::session::KIND_PAIRING), content)
        .tags([Tag::public_key(source.pubkey())])
        .sign_with_keys(keys)
        .unwrap()
}
fn entry() -> (Exchange, Keys, buzz_pairing::QrPayload, String) {
    let (session, qr) = PairingSession::new_source("wss://relay.test".into());
    let target = Keys::generate();
    let mut exchange = Exchange {
        session,
        payload: Some(Zeroizing::new("identity-fixture".into())),
        code_entry: false,
    };
    let offer = message(
        &target,
        &exchange.session,
        PairingMessage::Offer {
            session_id: hex::encode(crypto::derive_session_id(&qr.session_secret)),
            version: 1,
            confirmation: Some("desktop-code-v1".into()),
        },
    );
    let output = exchange.receive(&offer).unwrap();
    assert!(matches!(
        output.status,
        Some(Status::Code {
            code_entry: true,
            ..
        })
    ));
    assert_eq!(output.events.len(), 1);
    let Some(Status::Code { code, .. }) = output.status else {
        panic!("missing code")
    };
    (exchange, target, qr, code)
}
fn proof(exchange: &Exchange, target: &Keys, qr: &buzz_pairing::QrPayload) -> Event {
    let shared =
        nostr_pairing::util::generate_shared_key(target.secret_key(), &qr.source_pubkey).unwrap();
    let (_, sas) = crypto::derive_sas(&shared, &qr.session_secret);
    let hash = crypto::derive_transcript_hash(
        &crypto::derive_session_id(&qr.session_secret),
        &qr.source_pubkey.to_bytes(),
        &target.public_key().to_bytes(),
        &sas,
        &qr.session_secret,
    );
    message(
        target,
        &exchange.session,
        PairingMessage::SasConfirm {
            transcript_hash: hex::encode(hash),
        },
    )
}
fn submission(exchange: &Exchange, target: &Keys, code: &str, attempt: u8) -> Event {
    message(
        target,
        &exchange.session,
        PairingMessage::CodeSubmit {
            code: code.into(),
            request_id: attempt.to_string(),
        },
    )
}
#[test]
fn transcript_alone_never_exports_and_guess_budget_cannot_be_reset() {
    let (mut exchange, target, qr, code) = entry();
    assert!(
        exchange
            .receive(&proof(&exchange, &target, &qr))
            .unwrap()
            .events
            .is_empty()
    );
    assert!(exchange.payload.is_some());
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for attempt in 1..=5 {
        let event = submission(&exchange, &target, wrong, attempt);
        let output = exchange.receive(&event).unwrap();
        assert_eq!(output.events.len(), 1);
        assert!(exchange.payload.is_some());
        if attempt == 5 {
            assert!(matches!(output.status, Some(Status::Error { .. })));
        }
        if attempt < 5 {
            assert!(exchange.receive(&event).unwrap().events.is_empty());
        } else {
            assert!(exchange.receive(&event).is_err());
        }
    }
    assert!(
        exchange
            .receive(&submission(&exchange, &target, &code, 6))
            .is_err()
    );
    assert!(exchange.payload.is_some());
}
#[test]
fn timeout_preserves_possible_import_only_after_payload_publication() {
    let manager = Pairing::default();
    let (tx, _) = mpsc::channel(1);
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Transferring,
        payload_sent: false,
    });
    manager.expire("live");
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Expired
    );
    manager.mark_payload_sent("live");
    manager.expire("stale");
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Expired
    );
    manager.expire("live");
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Uncertain
    );
}
#[test]
fn entry_requires_phone_proof_and_real_completion() {
    let (mut exchange, target, qr, code) = entry();
    assert!(
        exchange.confirm().is_err(),
        "desktop cannot bypass code entry"
    );
    let output = exchange
        .receive(&submission(&exchange, &target, &code, 1))
        .unwrap();
    assert_eq!(output.status, Some(Status::Transferring));
    assert_eq!(output.events.len(), 2);
    let plain = Zeroizing::new(
        nostr_pairing::nips::nip44::decrypt(
            target.secret_key(),
            &qr.source_pubkey,
            &output.events[1].content,
        )
        .unwrap(),
    );
    assert!(plain.contains("identity-fixture"));
    assert!(!format!("{:?}", output.status).contains("identity-fixture"));
    let done = message(
        &target,
        &exchange.session,
        PairingMessage::Complete { success: true },
    );
    assert_eq!(
        exchange.receive(&done).unwrap().status,
        Some(Status::Complete)
    );
}
#[test]
fn mismatch_and_phone_import_failure_never_claim_success() {
    let (mut exchange, target, _, code) = entry();
    let wrong = message(
        &target,
        &exchange.session,
        PairingMessage::CodeSubmit {
            code: if code == "000000" { "000001" } else { "000000" }.into(),
            request_id: "wrong".into(),
        },
    );
    let rejection = exchange.receive(&wrong).unwrap();
    assert_eq!(rejection.events.len(), 1);
    assert_ne!(rejection.status, Some(Status::Transferring));
    assert!(exchange.payload.is_some());
    let (mut exchange, target, _qr, code) = entry();
    exchange
        .receive(&submission(&exchange, &target, &code, 1))
        .unwrap();
    let rejected = message(
        &target,
        &exchange.session,
        PairingMessage::Complete { success: false },
    );
    assert!(exchange.receive(&rejected).is_err());
}
#[test]
fn legacy_phone_still_needs_explicit_desktop_confirmation() {
    let (session, qr) = PairingSession::new_source("wss://relay.test".into());
    let (mut phone, offer) = PairingSession::new_target(&qr).unwrap();
    let mut exchange = Exchange {
        session,
        payload: Some(Zeroizing::new("fixture".into())),
        code_entry: false,
    };
    assert!(matches!(
        exchange.receive(&offer).unwrap().status,
        Some(Status::Code {
            code_entry: false,
            ..
        })
    ));
    let output = exchange.confirm().unwrap();
    phone.handle_sas_confirm(&output.events[0]).unwrap();
    phone.confirm_target_sas().unwrap();
    assert_eq!(
        &*phone.handle_payload(&output.events[1]).unwrap().1,
        "fixture"
    );
    assert_eq!(
        exchange
            .receive(&phone.send_complete().unwrap())
            .unwrap()
            .status,
        Some(Status::Complete)
    );
}
#[tokio::test]
async fn old_session_cleanup_cannot_cancel_or_overwrite_replacement() {
    let manager = Pairing::default();
    let (tx, _) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    *manager.0.lock().unwrap() = Some(Active {
        id: "new".into(),
        cancel: cancel.clone(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Connecting,
        payload_sent: false,
    });
    manager.cancel("old").await.unwrap();
    manager.update("old", Status::Complete);
    assert!(!cancel.is_cancelled());
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Connecting
    );
    manager
        .0
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .finished
        .cancel();
    manager.cancel("new").await.unwrap();
    assert!(cancel.is_cancelled());
    manager.update("new", Status::Complete);
    assert!(manager.0.lock().unwrap().is_none());
}

#[tokio::test]
async fn window_reload_or_destruction_cancels_the_live_attempt() {
    let manager = Pairing::default();
    let (tx, _) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: cancel.clone(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Connecting,
        payload_sent: false,
    });
    manager.cancel_all();
    assert!(cancel.is_cancelled());
    assert!(manager.0.lock().unwrap().is_none());
}

#[test]
fn cancellation_preserves_post_publication_outcomes() {
    for (before, after) in [
        (Status::Transferring, Status::Uncertain),
        (Status::Complete, Status::Complete),
        (
            Status::Error {
                message: "Your phone couldn’t save the account. Try pairing again.".into(),
            },
            Status::Error {
                message: "Your phone couldn’t save the account. Try pairing again.".into(),
            },
        ),
    ] {
        let manager = Pairing::default();
        let (tx, _) = mpsc::channel(1);
        *manager.0.lock().unwrap() = Some(Active {
            id: "live".into(),
            cancel: CancellationToken::new(),
            finished: CancellationToken::new(),
            confirm: tx,
            status: before,
            payload_sent: true,
        });
        // Window close, reload, and macOS hide all use this path.
        manager.cancel_all();
        let active = manager.0.lock().unwrap();
        let active = active.as_ref().unwrap();
        assert!(active.cancel.is_cancelled());
        assert_eq!(active.status, after);
    }
}

#[test]
fn cancellation_before_publication_blocks_the_payload() {
    let manager = Pairing::default();
    let (tx, _) = mpsc::channel(1);
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Transferring,
        payload_sent: false,
    });
    manager.cancel_all();
    assert!(!manager.mark_payload_sent("live"));
}

#[tokio::test]
async fn unexpected_connection_failure_becomes_a_visible_error() {
    let result = guard(async { panic!("simulated connection setup failure") }).await;
    assert_eq!(
        match result.unwrap_err() {
            Failure::Transport(message) => message,
            other => panic!("{other:?}"),
        },
        "Pairing stopped unexpectedly. Create a new code and try again."
    );
}

#[test]
fn transport_failure_is_uncertain_after_publication_but_phone_rejection_is_definite() {
    let manager = Pairing::default();
    let (tx, _) = mpsc::channel(1);
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Transferring,
        payload_sent: false,
    });
    let error = Status::Error {
        message: "connection lost".into(),
    };
    manager.fail("live", error.clone(), true);
    assert_eq!(manager.0.lock().unwrap().as_ref().unwrap().status, error);
    manager.mark_payload_sent("live");
    manager.fail("stale", error.clone(), true);
    assert_eq!(manager.0.lock().unwrap().as_ref().unwrap().status, error);
    manager.fail("live", error.clone(), true);
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Uncertain
    );
    manager.fail("live", error.clone(), false);
    assert_eq!(manager.0.lock().unwrap().as_ref().unwrap().status, error);
}

#[test]
fn mismatched_legacy_code_sends_user_denied_without_exporting() {
    let (session, qr) = PairingSession::new_source("wss://relay.test".into());
    let (mut phone, offer) = PairingSession::new_target(&qr).unwrap();
    let mut exchange = Exchange {
        session,
        payload: Some(Zeroizing::new("fixture".into())),
        code_entry: false,
    };
    exchange.receive(&offer).unwrap();
    let event = exchange
        .session
        .abort(AbortReason::UserDenied)
        .unwrap()
        .expect("legacy peer is known");
    assert_eq!(phone.handle_abort(&event).unwrap(), AbortReason::UserDenied);
    assert_eq!(exchange.session.state(), SessionState::Aborted);
    assert!(
        exchange.payload.is_some(),
        "denial must not release the identity"
    );
    assert!(exchange.confirm().is_err());
    let (mut code_entry, _, _, _) = entry();
    assert!(
        code_entry
            .session
            .abort(AbortReason::UserDenied)
            .unwrap()
            .is_some()
    );
    assert_eq!(code_entry.session.state(), SessionState::Aborted);
    assert!(code_entry.payload.is_some());
}

// Exercise the actual post-setup owner, not just PairingSession::abort.
async fn local_sockets() -> (
    relay::Socket,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
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

#[tokio::test]
async fn cancellation_notifies_a_known_peer_before_transfer_and_waits_for_teardown() {
    use futures::StreamExt;
    let (exchange, target, _, _) = entry();
    let (mut socket, mut server) = local_sockets().await;
    let (tx, mut rx) = mpsc::channel(1);
    let manager = Pairing::default();
    let cancel = CancellationToken::new();
    let finished = CancellationToken::new();
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: cancel.clone(),
        finished: finished.clone(),
        confirm: tx,
        status: Status::Code {
            code: "000000".into(),
            code_entry: true,
        },
        payload_sent: false,
    });
    let owner = manager.clone();
    let task = tokio::spawn(async move {
        let mut exchange = exchange;
        let mut auth = relay::Authentication::default();
        let result = exchange_until_deadline(
            ExchangeContext {
                pairing: &owner,
                id: "live",
                relay_url: &url::Url::parse("wss://relay.test").unwrap(),
                pending: vec![],
            },
            &mut exchange,
            &mut socket,
            &mut auth,
            &mut rx,
            &cancel,
            tokio::time::Instant::now() + Duration::from_secs(120),
        )
        .await;
        finished.cancel();
        result
    });
    manager.cancel("live").await.unwrap();
    assert!(task.is_finished(), "cancel returned before owner teardown");
    assert!(task.await.unwrap().is_ok());
    let frame = tokio::time::timeout(Duration::from_secs(2), server.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(json[0], "EVENT");
    let event = Event::from_json(json[1].to_string()).unwrap();
    let plaintext =
        nostr_pairing::nips::nip44::decrypt(target.secret_key(), &event.pubkey, &event.content)
            .unwrap();
    assert!(matches!(
        serde_json::from_str::<PairingMessage>(&plaintext).unwrap(),
        PairingMessage::Abort {
            reason: AbortReason::UserDenied
        }
    ));
}

#[tokio::test]
async fn cancellation_after_transfer_closes_without_aborting_the_importing_phone() {
    use futures::{SinkExt, StreamExt};
    let (exchange, target, _, code) = entry();
    let request = submission(&exchange, &target, &code, 1);
    let (socket, mut server) = local_sockets().await;
    let (tx, mut rx) = mpsc::channel(1);
    let manager = Pairing::default();
    let cancel = CancellationToken::new();
    let finished = CancellationToken::new();
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: cancel.clone(),
        finished: finished.clone(),
        confirm: tx,
        status: Status::Code {
            code: code.clone(),
            code_entry: true,
        },
        payload_sent: false,
    });
    let owner = manager.clone();
    let owner_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let mut exchange = exchange;
        let mut socket = socket;
        let mut auth = relay::Authentication::default();
        let result = exchange_until_deadline(
            ExchangeContext {
                pairing: &owner,
                id: "live",
                relay_url: &url::Url::parse("wss://relay.test").unwrap(),
                pending: vec![],
            },
            &mut exchange,
            &mut socket,
            &mut auth,
            &mut rx,
            &owner_cancel,
            tokio::time::Instant::now() + Duration::from_secs(120),
        )
        .await;
        finished.cancel();
        result
    });
    server
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["EVENT", "pair", request])
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let mut messages = vec![];
    for _ in 0..2 {
        let frame = tokio::time::timeout(Duration::from_secs(2), server.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(json[0], "EVENT");
        let event = Event::from_json(json[1].to_string()).unwrap();
        let plaintext =
            nostr_pairing::nips::nip44::decrypt(target.secret_key(), &event.pubkey, &event.content)
                .unwrap();
        messages.push(serde_json::from_str::<PairingMessage>(&plaintext).unwrap());
    }
    assert!(matches!(messages[1], PairingMessage::Payload { .. }));
    // Cancel before any status poll observes the transfer.
    assert_eq!(manager.cancel("live").await.unwrap(), Status::Uncertain);
    assert!(task.await.unwrap().is_ok());
    assert_eq!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Uncertain,
        "a later status read must not report an unsent cancellation"
    );
    assert!(
        !matches!(
            server.next().await,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
        ),
        "post-publication abort was sent"
    );
}

#[tokio::test]
async fn native_deadline_expires_without_a_peer_or_an_abort() {
    use futures::StreamExt;
    let (mut exchange, _, _, _) = entry();
    let (mut socket, mut server) = local_sockets().await;
    let (_tx, mut rx) = mpsc::channel(1);
    let mut auth = relay::Authentication::default();
    let result = exchange_until_deadline(
        ExchangeContext {
            pairing: &Pairing::default(),
            id: "live",
            relay_url: &url::Url::parse("wss://relay.test").unwrap(),
            pending: vec![],
        },
        &mut exchange,
        &mut socket,
        &mut auth,
        &mut rx,
        &CancellationToken::new(),
        tokio::time::Instant::now(),
    )
    .await;
    assert!(matches!(result, Err(Failure::Expired)));
    drop(socket);
    assert!(!matches!(
        server.next().await,
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
    ));
}

#[test]
fn denial_keeps_native_code_until_abort_has_finished() {
    let manager = Pairing::default();
    let (tx, mut rx) = mpsc::channel(1);
    *manager.0.lock().unwrap() = Some(Active {
        id: "live".into(),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        confirm: tx,
        status: Status::Code {
            code: "123456".into(),
            code_entry: false,
        },
        payload_sent: false,
    });
    manager.decide("live", Decision::Deny).unwrap();
    assert!(matches!(rx.try_recv(), Ok(Decision::Deny)));
    assert!(matches!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Code { .. }
    ));
    manager.fail(
        "live",
        Status::Error {
            message: "Pairing was canceled.".into(),
        },
        false,
    );
    assert!(matches!(
        manager.0.lock().unwrap().as_ref().unwrap().status,
        Status::Error { .. }
    ));
}

#[tokio::test]
async fn delayed_subscription_expires_before_sidecar_connection_cap() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut socket, mut server) = local_sockets().await;
    let relay_url = url::Url::parse("wss://relay.test/pair").unwrap();
    let (session, _) = PairingSession::new_source(relay_url.to_string());
    let subscription = tokio::spawn(async move {
        let (pending, auth) = relay::subscribe(&mut socket, &session, &relay_url)
            .await
            .unwrap();
        (socket, session, relay_url, pending, auth)
    });
    assert!(
        server
            .next()
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap()
            .contains("REQ")
    );
    assert!(!subscription.is_finished(), "QR must wait for EOSE");
    server
        .send(Message::Text(r#"["EOSE","pair"]"#.into()))
        .await
        .unwrap();
    let (mut socket, mut session, relay_url, pending, mut auth) = subscription.await.unwrap();
    session.start_source_lifetime();
    // Model five seconds consumed by the held EOSE without waiting on wall time.
    // Pause only after real subscription I/O, avoiding auto-advance during I/O.
    tokio::time::pause();
    let connection_started = tokio::time::Instant::now() - Duration::from_secs(5);
    let deadline = exchange_deadline(&session, connection_started);
    assert_eq!(
        deadline - tokio::time::Instant::now(),
        Duration::from_secs(110)
    );
    let task = tokio::spawn(async move {
        let mut exchange = Exchange {
            session,
            payload: Some(Zeroizing::new("fixture".into())),
            code_entry: false,
        };
        let (_tx, mut rx) = mpsc::channel(1);
        exchange_until_deadline(
            ExchangeContext {
                pairing: &Pairing::default(),
                id: "live",
                relay_url: &relay_url,
                pending,
            },
            &mut exchange,
            &mut socket,
            &mut auth,
            &mut rx,
            &CancellationToken::new(),
            deadline,
        )
        .await
    });
    tokio::time::advance(Duration::from_secs(109)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(task.await.unwrap(), Err(Failure::Expired)));
    // The native expiry maps to Expired (covered above), which the existing UI
    // renewal regression consumes. The sidecar cap has not arrived yet.
    assert!(tokio::time::Instant::now() < connection_started + Duration::from_secs(120));
    tokio::time::advance(Duration::from_secs(5)).await;
    drop(server);
}

#[tokio::test]
async fn early_connection_loss_is_error_before_publication_and_uncertain_after() {
    for payload_sent in [false, true] {
        let (mut socket, server) = local_sockets().await;
        let (session, _) = PairingSession::new_source("wss://relay.test".into());
        let manager = Pairing::default();
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        *manager.0.lock().unwrap() = Some(Active {
            id: "live".into(),
            cancel: cancel.clone(),
            finished: CancellationToken::new(),
            confirm: tx,
            status: Status::Qr {
                matrix: Arc::new(QrMatrix::encode("fixture").unwrap()),
                uri: "fixture".into(),
            },
            payload_sent,
        });
        let deadline = exchange_deadline(&session, tokio::time::Instant::now());
        let mut exchange = Exchange {
            session,
            payload: Some(Zeroizing::new("fixture".into())),
            code_entry: false,
        };
        drop(server);
        let result = exchange_until_deadline(
            ExchangeContext {
                pairing: &manager,
                id: "live",
                relay_url: &url::Url::parse("wss://relay.test").unwrap(),
                pending: vec![],
            },
            &mut exchange,
            &mut socket,
            &mut relay::Authentication::default(),
            &mut rx,
            &cancel,
            deadline,
        )
        .await;
        let Err(Failure::Transport(message)) = result else {
            panic!("unexpected result: {result:?}");
        };
        manager.fail("live", Status::Error { message }, true);
        let active = manager.0.lock().unwrap();
        let status = &active.as_ref().unwrap().status;
        if payload_sent {
            assert_eq!(status, &Status::Uncertain);
        } else {
            assert!(matches!(status, Status::Error { .. }));
        }
    }
}

#[tokio::test]
async fn sign_out_blocks_a_prepared_payload_and_any_later_attempt() {
    use futures::{SinkExt, StreamExt};
    let (exchange, target, _, code) = entry();
    let request = submission(&exchange, &target, &code, 1);
    let (socket, mut server) = local_sockets().await;
    let (tx, mut rx) = mpsc::channel(1);
    let manager = Pairing::default();
    let cancel = CancellationToken::new();
    let finished = CancellationToken::new();
    let attempt = |id: &str, cancel: CancellationToken, finished, confirm| Active {
        id: id.into(),
        cancel,
        finished,
        confirm,
        status: Status::Code {
            code: code.clone(),
            code_entry: true,
        },
        payload_sent: false,
    };
    manager
        .admit(attempt("live", cancel.clone(), finished.clone(), tx))
        .unwrap();
    // The payload is prepared and waits only for the phone's code.
    manager.close();
    let owner = manager.clone();
    let task = tokio::spawn(async move {
        let mut exchange = exchange;
        let mut socket = socket;
        let mut auth = relay::Authentication::default();
        let result = exchange_loop(
            ExchangeContext {
                pairing: &owner,
                id: "live",
                relay_url: &url::Url::parse("wss://relay.test").unwrap(),
                pending: vec![],
            },
            &mut exchange,
            &mut socket,
            &mut auth,
            &mut rx,
        )
        .await;
        finished.cancel();
        result
    });
    server
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["EVENT", "pair", request])
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    while let Ok(Some(Ok(frame))) =
        tokio::time::timeout(Duration::from_millis(500), server.next()).await
    {
        let Ok(text) = frame.to_text() else { continue };
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let event = Event::from_json(json[1].to_string()).unwrap();
        let plaintext =
            nostr_pairing::nips::nip44::decrypt(target.secret_key(), &event.pubkey, &event.content)
                .unwrap();
        assert!(
            !matches!(
                serde_json::from_str::<PairingMessage>(&plaintext).unwrap(),
                PairingMessage::Payload { .. }
            ),
            "the key was published after sign-out"
        );
    }
    let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
    assert!(ended.unwrap().unwrap().is_ok());
    // A start raced past the close is refused, not registered.
    let (tx, _) = mpsc::channel(1);
    let late = CancellationToken::new();
    assert!(
        manager
            .admit(attempt("late", late.clone(), CancellationToken::new(), tx))
            .is_err()
    );
    assert!(!manager.mark_payload_sent("late"));
}
